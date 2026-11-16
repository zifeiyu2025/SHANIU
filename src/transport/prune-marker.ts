/**
 * 确定性裁剪的判读文案（N7）。
 *
 * 界面（重开会话时的卡片）与导出（JSONL / Markdown）必须用同一份措辞：两边各写
 * 一套迟早一边改一边没改，导出就会把档案里的完整原文冒充成「模型看到的内容」。
 *
 * 只陈述事实——原文多长、头尾各留多少、中段省略多少——并明说下文是**裁剪前原文**。
 * 档案从不重写工具输出，界面展示的也是原文；不说清楚，读者会以为模型拿到了全部。
 */
import type { PruneInfoWire } from './types'

/** 「已裁剪」前缀行，用于卡片正文与导出正文（后接裁剪前原文）。 */
export function pruneMarker(info: PruneInfoWire): string {
  return `[已裁剪] 这条工具输出在发给模型前被确定性裁剪：原始 ${info.original_chars} 字符，保留头 ${info.head_chars} + 尾 ${info.tail_chars}，中段省略 ${info.omitted_chars} 字符。以下为裁剪前原文（完整）。`
}

/** 给裁剪前原文加判读行；未裁剪原样返回。 */
export function decorateToolOutput(output: string, pruned?: PruneInfoWire): string {
  return pruned === undefined || pruned === null ? output : `${pruneMarker(pruned)}\n\n${output}`
}

/**
 * 实时提示文案：裁剪发生在工具调用之后的回合里，那张卡片早已渲染成完整原文，
 * 不能事后回改；用一条通知说明「刚才那条输出模型只看到裁剪版」。
 */
export function pruneNoticeText(
  callId: string,
  tool: string | undefined,
  info: PruneInfoWire,
): string {
  const who = tool === undefined ? `call_id ${callId}` : `工具 ${tool}（call_id ${callId}）`
  return `${who} 的输出在发给模型前被确定性裁剪：原始 ${info.original_chars} 字符，保留头 ${info.head_chars} + 尾 ${info.tail_chars}，中段省略 ${info.omitted_chars} 字符。卡片里显示的仍是裁剪前原文。`
}
