/**
 * MCP 信任设置页的纯逻辑（无 React、无 IPC）：默认勾选、裁决载荷与展示格式。
 *
 * 这些函数是 UI 与宿主契约之间唯一的形状转换点，单独测试比渲染快照更能兜住
 * 「默认 destructive」「未勾选不批准」「显式启用必须真传下去」这三条口径。
 */
import type {
  McpRiskLevel,
  ReviewPayloadWire,
  ReviewToolWire,
  TrustDecisionWire,
} from '../transport/mcp-wire'

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
