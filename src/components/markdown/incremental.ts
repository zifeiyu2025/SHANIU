import type { Root, RootContent } from 'mdast';

/**
 * 追加式流的块级增量解析（判据照抄 dsh ui-primitives/src/markdown/incremental.ts）。
 *
 * 每个 chunk 重解析整段回复，代价是最终长度的平方。CommonMark 的块解析按行进行，
 * 追加的文本只可能重塑**解析前沿**——最后一个顶层块（段落变成 setext 标题或表格、
 * 列表在空行后继续），更早的块已经定稿。所以这里只冻结除尾部
 * {@link UNSTABLE_TAIL_BLOCKS} 块之外的块，每帧只重解析它们之后的源文本。
 *
 * 切点取**最后一个被冻结块的 end offset**，而不是下一块的 start offset：后一块的
 * start 会跳过最多三个空格的无意义缩进，切在前一块的 end 才能把块间空行留在尾部，
 * 保证切出来的源文本逐字不变。
 *
 * 未移植的偏差（有据）：dsh 还对「未闭合代码围栏」做了逐行增量（openFence 分支），
 * 让围栏内已完成的行不再重复过语法。那是纯性能优化，不影响正确性，也不在 FF-7
 * 「已完成块重渲染 = 0」的门禁范围内，故本卡不搬；长代码块流式期间的重解析代价
 * 落在尾部块上，等该块定稿即冻结。
 */

/** 尾部保持不稳定的块数：追加文本最多重塑最后一块，留一块作安全边界。 */
const UNSTABLE_TAIL_BLOCKS = 2;

/** 一个顶层块 + 跨 chunk 稳定的渲染 key + 它的源文本。 */
export interface PositionedBlock {
  /** 块节点，其内部 position 相对于本次解析的切片。 */
  readonly node: RootContent;
  /** 块在完整源文本里的起始偏移；跨冻结边界不变，React 因此复用而非重挂。 */
  readonly key: number;
  /** 块的源文本，供冻结缓存的渲染回调与调试使用。 */
  readonly source: string;
}

export interface IncrementalBlocks {
  /** 不会再变的块，每个代次单调增长。 */
  readonly frozen: readonly PositionedBlock[];
  /** 本帧重解析的不稳定尾部。 */
  readonly tail: readonly PositionedBlock[];
  /** 非追加输入时自增，调用方据此丢弃按它建键的缓存。 */
  readonly generation: number;
}

/** 块的渲染 key：绝对源起始偏移；无 position 的节点退化成负序号。 */
function blockKey(node: RootContent, base: number, index: number): number {
  const offset = node.position?.start.offset;
  return offset === undefined ? -(index + 1) : base + offset;
}

export class IncrementalMarkdownParser {
  private prevText = '';
  private tailStart = 0;
  private frozen: PositionedBlock[] = [];
  private generation = 0;
  private cached: IncrementalBlocks | null = null;

  /** @param parse - 与渲染层共用的同一套语法，块边界才不会对不上。 */
  constructor(private readonly parse: (text: string) => Root) {}

  /**
   * 折入当前累计文本，返回冻结/尾部切分。同文本重复调用返回同一结果，
   * 因此 React 渲染路径可以随意重跑。
   */
  update(text: string): IncrementalBlocks {
    if (this.cached !== null && text === this.prevText) return this.cached;
    // 每帧做一次 O(prefix) 的前缀比较：可靠的「是否还是追加」必须校验整个保留前缀，
    // 而 startsWith 比解析快两个数量级——这正是本类存在的理由。
    if (!text.startsWith(this.prevText)) {
      this.prevText = '';
      this.tailStart = 0;
      this.frozen = [];
      this.generation += 1;
    }
    this.prevText = text;
    const base = this.tailStart;
    const source = text.slice(base);
    const blocks = this.parse(source).children;
    let firstUnstable = Math.max(0, blocks.length - UNSTABLE_TAIL_BLOCKS);
    if (firstUnstable > 0) {
      const cutEnd = blocks[firstUnstable - 1]?.position?.end.offset;
      if (cutEnd === undefined) {
        // 语法没给 position 就没有切点，整段留在尾部而不是猜一个边界。
        firstUnstable = 0;
      } else {
        for (const [index, node] of blocks.slice(0, firstUnstable).entries()) {
          this.frozen.push({
            node,
            key: blockKey(node, base, index),
            source: sliceSource(source, node),
          });
        }
        this.tailStart = base + cutEnd;
      }
    }
    const tailSource = text.slice(this.tailStart);
    const tail: PositionedBlock[] = blocks.slice(firstUnstable).map((node, index) => ({
      node,
      key: blockKey(node, base, this.frozen.length + index),
      source: sliceSource(tailSource, node),
    }));
    this.cached = { frozen: [...this.frozen], tail, generation: this.generation };
    return this.cached;
  }
}

/** 块的源文本；缺 position 时退回整片，调用方只把它用于回调。 */
function sliceSource(source: string, node: RootContent): string {
  const start = node.position?.start.offset;
  const end = node.position?.end.offset;
  return start === undefined || end === undefined ? source : source.slice(start, end);
}
