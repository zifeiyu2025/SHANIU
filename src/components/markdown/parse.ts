import type { Root } from 'mdast';
import { fromMarkdown } from 'mdast-util-from-markdown';
import { gfmFromMarkdown } from 'mdast-util-gfm';
import { gfm } from 'micromark-extension-gfm';

/** GFM 删除线只认 `~~`；单个 `~` 保持字面，避免把 `10~20` 这类区间写成删除线。 */
const gfmOptions = { singleTilde: false };

/**
 * 会话流的唯一语法：GFM，不含数学。
 * 流式与收口用同一套语法，增量切分出来的块边界才和整段解析一致。
 */
export function parseGfm(text: string): Root {
  return fromMarkdown(text, {
    extensions: [gfm(gfmOptions)],
    mdastExtensions: [gfmFromMarkdown()],
  });
}
