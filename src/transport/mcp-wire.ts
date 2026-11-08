/**
 * SH-063 MCP 信任面的前端镜像：应用级事件（`shaniu://mcp`）与 6 个宿主命令。
 *
 * 约定与 `types.ts` 一致：字段名跟 Rust serde 的 snake_case 走，不做重命名；
 * 事件判别值是 `McpEventWire` 的 `type` 字面量，与 Rust `McpEvent` 的
 * `#[serde(tag = "type", rename_all = "snake_case")]` 机械对应
 * （契约由 `src-tauri/tests/mcp_event_schema.rs` 双向校验）。
 *
 * 安全口径：`server_read_only_hint` 只是服务器自报值的**展示**，
 * 风险档位永远取 `risk`（用户批准记录，ADR-107）。
 */
import type { HostBridge } from './tauri'

/** 运行时状态（Rust `McpStatus`）。 */
export type McpStatus = 'idle' | 'review' | 'trusted' | 'blocked' | 'retrust' | 'degraded' | 'failed'

/** 风险档位（Rust `events::RiskLevel`）。 */
export type McpRiskLevel = 'read' | 'write_in_workspace' | 'execute' | 'destructive' | 'plan_review'

/** 注入命中（逐字片段，已去控制字符）。 */
export interface GuardHitView {
  readonly rule: string
  readonly fragment: string
}

/** 审查界面上的一件工具。 */
export interface ReviewToolWire {
  readonly dispatch_name: string
  readonly raw_name: string
  readonly description: string
  readonly schema: unknown
  readonly annotations?: unknown
  /** server 自称只读——只作展示，不参与风险决策。 */
  readonly server_read_only_hint: boolean
  /** 用户选定的档位（默认 destructive）。 */
  readonly risk: McpRiskLevel
  readonly approved: boolean
  /** 该授权作用于哪个指纹（界面要能看出「这条授权是给谁的」）。 */
  readonly fingerprint: string
  readonly guard_hits: readonly GuardHitView[]
  readonly guard_explanation?: string
  /** 已通过显式启用绕过注入拦截。 */
  readonly guard_exempted: boolean
}

export interface RejectedToolWire {
  readonly raw_name: string
  readonly reason: string
}

export interface ApprovedToolWire {
  readonly dispatch_name: string
  readonly risk: McpRiskLevel
}

/** 一条显式启用记录（可撤销，且带指纹与命中片段供审计）。 */
export interface ExemptionWire {
  readonly server_id: string
  readonly fingerprint: string
  readonly dispatch_name: string
  readonly rule: string
  readonly hit_fragment: string
  readonly granted_at_ms: number
}

/** 一个 server 的沙箱口径（与状态条、日志同源，后端只算一次）。 */
export interface SandboxSummaryWire {
  /** `app_container` | `logical` | `unsandboxed` */
  readonly kind: string
  /** 容器 profile 名（仅 app_container 非空）。 */
  readonly profile?: string
  /** 工作区授权档位：`read` | `write`；unsandboxed 为空。 */
  readonly workspace?: string
  /** 额外目录授权摘要，形如 `读：D:\data`。 */
  readonly allowed_roots?: readonly string[]
  /** `allowed` | `denied` | `not_isolated`（逻辑沙箱没有 OS 网络隔离）。 */
  readonly network: string
}

/** 首连审查（也是信任状态查询）的完整载荷。 */
export interface ReviewPayloadWire {
  readonly server_id: string
  readonly status: McpStatus
  readonly fingerprint: string
  readonly transport: string
  readonly executable: string
  readonly executable_sha256: string
  /** 可执行文件当前修改时间（ms）；只在「需要重新信任」时给出。 */
  readonly executable_mtime_ms?: number
  /** 上次批准时记录的可执行文件哈希；只在「需要重新信任」时给出。 */
  readonly previous_executable_sha256?: string
  /** 上次批准时间（ms）。 */
  readonly approved_at_ms: number
  /** `tools` 来自旧版本缓存，仅作参考，不是当前有效清单。 */
  readonly tools_stale: boolean
  readonly argv: readonly string[]
  readonly cwd: string
  /** `KEY = sha256:xxxxxxxx…`，值永不明文。 */
  readonly env: readonly string[]
  readonly protocol_version: string
  readonly server_name: string
  readonly server_version: string
  readonly allow_network: boolean
  readonly unsandboxed: boolean
  readonly tools: readonly ReviewToolWire[]
  readonly rejected_tools: readonly RejectedToolWire[]
  readonly approved_tools: readonly ApprovedToolWire[]
  readonly exemptions: readonly ExemptionWire[]
  /** 指纹覆盖范围：`interpreter_only` 或 `module_tree`（解释器 + 模块代码树）。 */
  readonly coverage?: CoverageWire
  /** 沙箱口径（与状态条、日志同源）。 */
  readonly sandbox?: SandboxSummaryWire
  readonly warnings: readonly string[]
  readonly reason?: string
}

/** 指纹覆盖范围。两种范围的安全性不同，界面必须分开显示。 */
export interface CoverageWire {
  readonly kind: 'interpreter_only' | 'module_tree'
  readonly detail: string
}

/** 审查界面逐工具的裁决输入。 */
export interface TrustDecisionWire {
  readonly dispatch_name: string
  readonly approved: boolean
  readonly risk?: McpRiskLevel
  /** 工具被注入规则命中时，用户看完命中片段后的显式启用。 */
  readonly enable_injection?: boolean
}

/** 设置页列表项（配置 + 运行时状态合并）。 */
export interface McpServerSummaryWire {
  readonly id: string
  readonly enabled: boolean
  readonly command: string
  readonly args: readonly string[]
  readonly unsandboxed: boolean
  readonly allow_network: boolean
  /** `idle | review | trusted | blocked | retrust | degraded | failed | disabled`。 */
  readonly status: string
  readonly fingerprint: string | null
  readonly approved_tools: readonly ApprovedToolWire[]
  /** 用户显式启用的注入豁免数量。 */
  readonly exemption_count: number
  readonly reason: string | null
}

/** 应用级 MCP 事件（`shaniu://mcp`）。 */
export type McpEventWire =
  | {
      readonly type: 'server_status_changed'
      readonly server_id: string
      readonly status: McpStatus
      readonly reason?: string
    }
  | {
      readonly type: 'manifest_review'
      readonly server_id: string
      readonly review: ReviewPayloadWire
    }
  | {
      readonly type: 'trust_resolved'
      readonly server_id: string
      readonly fingerprint: string
      readonly status: McpStatus
      readonly tools: readonly ApprovedToolWire[]
    }
  | {
      readonly type: 'exemption_changed'
      readonly server_id: string
      readonly fingerprint: string
      readonly exemptions: readonly ExemptionWire[]
    }

/** MCP 事件通道名（与 Rust `MCP_EVENT_CHANNEL` 一致）。 */
export const MCP_EVENT_CHANNEL = 'shaniu://mcp'

/** MCP 事件订阅源（桌面运行时用 Tauri，测试注入脚本化实现）。 */
export interface McpEventSource {
  listen(handler: (event: McpEventWire) => void): () => void
}

/** 桌面运行时事件源：懒加载 Tauri API，与 `tauriBridge().listenEvents` 同构。 */
export function tauriMcpSource(): McpEventSource {
  return {
    listen(handler: (event: McpEventWire) => void): () => void {
      let unlisten: (() => void) | undefined
      let closed = false
      void import('@tauri-apps/api/event')
        .then(({ listen }) => listen<McpEventWire>(MCP_EVENT_CHANNEL, ({ payload }) => handler(payload)))
        .then((off) => {
          unlisten = off
          if (closed) off()
        })
        .catch((reason: unknown) => {
          console.error('[SHANIU] MCP 事件通道订阅失败，信任状态不会实时更新', reason)
        })
      return () => {
        closed = true
        unlisten?.()
      }
    },
  }
}

export function mcpListServers(bridge: HostBridge): Promise<McpServerSummaryWire[]> {
  return bridge.invoke<McpServerSummaryWire[]>('mcp_list_servers')
}

/** 全部显式启用记录（插件页「豁免」tab 的只读台账）。 */
export function mcpListExemptions(bridge: HostBridge): Promise<ExemptionWire[]> {
  return bridge.invoke<ExemptionWire[]>('mcp_list_exemptions')
}

/**
 * 首连审查：未批准前宿主不会注册任何工具。
 *
 * `acceptNewBinary` 只由「需要重新信任」界面的显式确认传入。可执行文件哈希与批准记录
 * 不一致时，宿主默认**不启动进程**，只返回 `retrust` 载荷（上次哈希 vs 当前哈希、
 * 旧清单仅供参考）。
 */
export function mcpConnectAndReview(
  bridge: HostBridge,
  serverId: string,
  acceptNewBinary = false,
): Promise<ReviewPayloadWire> {
  return bridge.invoke<ReviewPayloadWire>('mcp_connect_and_review', { serverId, acceptNewBinary })
}

/** 应用裁决：批准位 + 逐工具档位 + 注入工具的显式启用。 */
export function mcpResolveTrust(
  bridge: HostBridge,
  serverId: string,
  fingerprint: string,
  decisions: readonly TrustDecisionWire[],
): Promise<ReviewPayloadWire> {
  return bridge.invoke<ReviewPayloadWire>('mcp_resolve_trust', { serverId, fingerprint, decisions })
}

/** 撤销整个 server 的信任与豁免（立即失效，下次连接重新审查）。 */
export function mcpRevokeTrust(bridge: HostBridge, serverId: string): Promise<void> {
  return bridge.invoke<void>('mcp_revoke_trust', { serverId })
}

/** 撤销单个注入豁免（工具立即不可调用）；`fingerprint` 缺省作用于当前指纹。 */
export function mcpRevokeExemption(
  bridge: HostBridge,
  serverId: string,
  dispatchName: string,
  fingerprint?: string,
): Promise<void> {
  return bridge.invoke<void>('mcp_revoke_exemption', { serverId, dispatchName, fingerprint })
}
