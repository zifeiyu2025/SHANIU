/** 头尾截断的度量（照抄 dsh ui-primitives/src/head-tail-cap.ts，纯算术、可单测）。 */
export interface HeadTailCap {
  /** 超出上限的行数（总行数 − maxLines）；≤ 0 表示没有隐藏行。 */
  hidden: number;
  /** 超上限且未展开 → 该渲染头尾切片。 */
  capped: boolean;
  /** 头部行数：ceil(maxLines / 2)。 */
  headLines: number;
  /** 尾部行数：上限减去头部。 */
  tailLines: number;
}

export function headTailCap(total: number, maxLines: number, expanded: boolean): HeadTailCap {
  const hidden = total - maxLines;
  const headLines = Math.ceil(maxLines / 2);
  return { hidden, capped: hidden > 0 && !expanded, headLines, tailLines: maxLines - headLines };
}

/**
 * 按 \n 切行：末尾换行是终止符不是空行（dsh TerminalBlock.tsx:217-219 同口径），
 * 否则每条命令都会多出一行空白并被算进"其余 N 行"。
 */
export function splitLines(text: string): string[] {
  const body = text.endsWith('\n') ? text.slice(0, -1) : text;
  return body.split('\n');
}
