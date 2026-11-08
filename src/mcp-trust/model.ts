/**
 * MCP 信任设置页的纯逻辑（无 React、无 IPC）：默认勾选、裁决载荷与展示格式。
 *
 * 这些函数是 UI 与宿主契约之间唯一的形状转换点，单独测试比渲染快照更能兜住
 * 「默认 destructive」「未勾选不批准」「显式启用必须真传下去」这三条口径。
 */
import type {
  McpRiskLevel,
  McpServerSummaryWire,
  ReviewPayloadWire,
  ReviewToolWire,
  TrustDecisionWire,
} from '../transport/mcp-wire'
import type { McpTrustKey } from './locales'

/** 界面上一个工具的裁决状态（受控表单值）。 */
export interface ToolSelection {
  readonly approved: boolean
  readonly risk: McpRiskLevel
  readonly enableInjection: boolean
}

/** 用户可选的档位（`plan_review` 是内部计划态，不由用户指定）。 */
export const SELECTABLE_RISKS: readonly McpRiskLevel[] = [
  'read',
  'write_in_workspace',
  'execute',
  'destructive',
]

/** 默认勾选：沿用信任记录里的批准位；没记录一律不勾、档位 destructive。 */
export function defaultSelections(review: ReviewPayloadWire): Record<string, ToolSelection> {
  const selections: Record<string, ToolSelection> = {}
  for (const tool of review.tools) {
    selections[tool.dispatch_name] = {
      approved: tool.approved,
      risk: tool.risk === 'plan_review' ? 'destructive' : tool.risk,
      enableInjection: tool.guard_exempted,
    }
  }
  return selections
}

/** 生成裁决载荷：`enable_injection` 只在已批准时下传，避免误导性授权。 */
export function buildDecisions(
  review: ReviewPayloadWire,
  selections: Record<string, ToolSelection>,
): TrustDecisionWire[] {
  return review.tools.map((tool) => {
    const selection = selections[tool.dispatch_name] ?? {
      approved: false,
      risk: 'destructive' as const,
      enableInjection: false,
    }
    return {
      dispatch_name: tool.dispatch_name,
      approved: selection.approved,
      risk: selection.risk,
      enable_injection: selection.approved && selection.enableInjection,
    }
  })
}

/** 勾选数量（按钮可用性用）。 */
export function approvedCount(selections: Record<string, ToolSelection>): number {
  return Object.values(selections).filter((selection) => selection.approved).length
}

/** 工具是否需要用户额外确认注入命中。 */
export function needsInjectionConsent(tool: ReviewToolWire): boolean {
  return tool.guard_hits.length > 0 && !tool.guard_exempted
}

/** 指纹缩写（完整值在详情里给全）。 */
export function shortFingerprint(fingerprint: string): string {
  return fingerprint.length > 16 ? `${fingerprint.slice(0, 16)}…` : fingerprint
}

/**
 * 卡片摘要的状态聚合（插件页列表层唯一的位置）。
 *
 * 顺序固定为「需要人处理的在前，稳定的在后」，同类状态合并成一段文字；
 * 0 台的状态不出现，避免摘要被无信息的段位撑长。
 */
const SUMMARY_ORDER = [
  'review',
  'retrust',
  'blocked',
  'failed',
  'degraded',
  'trusted',
  'idle',
  'disabled',
] as const

/** 摘要里每个状态段落的词典键。 */
type SummaryStateKey = `summary.${typeof SUMMARY_ORDER[number]}`

/**
 * 把服务器列表压成一行卡片摘要。
 * @param servers - 宿主返回的配置项与运行状态。
 * @param t - 本命名空间的翻译函数（只用到 `summary.*`）。
 * @returns 形如「3 台服务器 · 1 台待审查 · 1 台需要重新信任」的文本。
 */
export function summarizeServers(
  servers: readonly McpServerSummaryWire[],
  t: (key: McpTrustKey, params: { count: number }) => string,
): string {
  if (servers.length === 0) return t('summary.none', { count: 0 })
  const counts = new Map<string, number>()
  for (const server of servers) {
    counts.set(server.status, (counts.get(server.status) ?? 0) + 1)
  }
  const parts = [t('summary.total', { count: servers.length })]
  for (const status of SUMMARY_ORDER) {
    const count = counts.get(status) ?? 0
    if (count > 0) parts.push(t(`summary.${status}` as SummaryStateKey, { count }))
  }
  return parts.join(' · ')
}
