import type { ConversationNode } from '@deepseek-ai/dsh-client-ui-conversation/client'

/** 消息内容里的文本块拼成段落；图片与附件不进 Markdown。 */
function contentText(content: readonly { type: string; text?: string }[]): string {
  const texts: string[] = []
  for (const block of content) {
    if (block.type === 'text' && typeof block.text === 'string' && block.text !== '') texts.push(block.text)
  }
  return texts.join('\n\n')
}

/**
 * 会话正文 → Markdown。
 *
 * 只取客户端视图里已经落定的节点：用户消息、助手文本、思考（引用块）与工具调用行；
 * 与界面看到的内容一致，不额外读取宿主数据。
 * @param title - 会话标题，作为一级标题。
 * @param nodes - Chat 视图的会话节点。
 * @returns Markdown 文本。
 */
export function sessionToMarkdown(title: string, nodes: readonly ConversationNode[]): string {
  const parts: string[] = [`# ${title}`]
  for (const node of nodes) {
    if (node.kind === 'user' || node.kind === 'steering') {
      const text = contentText(node.content)
      if (text !== '') parts.push(`## 用户\n\n${text}`)
      continue
    }
    if (node.kind !== 'assistant') continue
    const chunks: string[] = []
    for (const block of node.blocks) {
      if (block.kind === 'text') chunks.push(block.text)
      else if (block.kind === 'reasoning') chunks.push(`> ${block.text}`)
      else if (block.kind === 'tool-call') chunks.push(`- 调用 \`${block.name}\`：${block.argsRaw}`)
    }
    const body = chunks.join('\n\n')
    if (body !== '') parts.push(`## 助手\n\n${body}`)
  }
  return `${parts.join('\n\n')}\n`
}
