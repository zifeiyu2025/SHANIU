/**
 * 子代理泳道的 wire 契约单点定义（SH-109 前端先行）。
 *
 * 上游 DSH 的子代理事实源是「每个子代理一个子会话」：父 journal 记
 * `subagent/catalog`、子 journal 记 `subagent/descriptor`，投影值
 * `subagentCatalog` / `subagentTiming` 由这些事件折叠（packages/subagent/
 * subagent/src/{catalog,descriptor,projection}.ts）。SHANIU 的后端没有子会话
 * ——子代理是 agent_loop tokio task，事件经 `AgentEvent{agent_id,parent_id}`
 * 信封到达——所以适配层在 transport 内合成泳道 journal：agent_id 直接充当
 * childSessionId（ULID 全局唯一），父 journal 落 catalog 事件、子 journal 落
 * turn/step/delta/tool 事件流。
 *
 * 本模块把这些**上游形状**收口成单点定义，mock（浏览器演示数据源）与 adapter
 * （桌面真实数据源）共用，防止双实现漂移：
 * - catalog 事件 data（v1 unknown / v3 continuable，zod schema 见上游 catalog.ts；
 *   v3 的 `unknown` 不合法——"只见过活动、没拿到 spawn 元数据"的懒建泳道诚实写 v1）；
 * - descriptor 事件 data（v3 continuable，上游 descriptor.ts）；
 * - `subagentTiming` 的折叠语义（上游 projection.ts 逐分支对齐，含 descriptor
 *   前的 pendingTurnStart 提升）；
 * - agent name 合法性（上游 names.ts 的 NAME_PATTERN，transport 层不进 host
 *   编译域，本地复制）。
 *
 * EventMsg 协议面（`subagent_spawned` 等三个新消息）不在本模块——FF-2 镜像以
 * Rust `events.rs` 为源，TS 单侧加变体会打红镜像检查，归 SH-053 与 Rust 同批。
 */
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type { SubagentCatalogEntry, SubagentTimingProjection } from '@deepseek-ai/dsh-subagent/client'

export type { SubagentCatalogEntry, SubagentTimingProjection }

/** 父 journal `subagent/catalog` 事件的 data（本层会写的两个版本；形状照上游 catalog.ts）。 */
export type SubagentCatalogEventData =
  | {
    /** v1：见过子代理活动但没拿到 spawn 元数据（懒建泳道的诚实口径）。 */
    readonly version: 1
    readonly childId: SessionId
    readonly childCreatedAt: number
    readonly mode: 'unknown'
    readonly label?: string
  }
  | {
    /** v3：spawn 元数据齐备的可续聊子代理（SH-053 接上 SubagentSpawned 后写入）。 */
    readonly version: 3
    readonly childId: SessionId
    readonly childCreatedAt: number
    readonly mode: 'continuable'
    readonly label: string
    readonly name: string
  }

/** 子 journal `subagent/descriptor` 事件的 data（v3 continuable，上游 descriptor.ts）。 */
export interface SubagentDescriptorEventData {
  readonly version: 3
  readonly mode: 'continuable'
  /** 建立该子代理的 provider 名（上游 `ctx.subagents` 的 provider；SHANIU 用固定标识）。 */
  readonly provider: string
  /** 创建标签（continuable 必带）。 */
  readonly label: string
  readonly agentProvider?: string
  readonly agentModel?: string
  readonly agentReasoningEffort?: string
}

/**
 * 泳道的 childSessionId：直接用后端 `agent_id`（ULID 全局唯一，可跨重启稳定重建）。
 * mock 的演示泳道同样用唯一字符串，遵守同一规则。
 */
export const laneIdOf = (agentId: string): SessionId => agentId as SessionId

/** catalog 事件 → 投影条目（去 version/childId/childCreatedAt、改名 id/createdAt；上游 catalog.ts 的 fold）。 */
export function catalogEntryOf(data: SubagentCatalogEventData): SubagentCatalogEntry {
  const { version: _version, childId, childCreatedAt, ...descriptor } = data
  return { id: childId, createdAt: childCreatedAt, ...descriptor } as SubagentCatalogEntry
}

/** 上游 names.ts 的 NAME_PATTERN（子代理名：小写字母开头的 1–32 位 [a-z0-9-]，`lead` 为根保留）。 */
const LANE_NAME_PATTERN = /^[a-z][a-z0-9-]{0,31}$/u

/** 是否为合法子代理名（catalog v3 的 `name` 必须过这道校验；`lead` 为根代理保留）。 */
export function isLaneName(value: string): boolean {
  return LANE_NAME_PATTERN.test(value) && value !== 'lead'
}

// ---- subagentTiming 折叠（上游 projection.ts 的 SHANIU 等价物）----

/** timing 折叠状态（与上游 TimingState 同形；zod 换成纯 TS，transport 不引 zod）。 */
export interface SubagentTimingState {
  settledMs: number
  active?: { since: number; through: number } | undefined
  /** descriptor 前的回合起点，descriptor 到达时提升为 active.since（上游语义）。 */
  pendingTurnStart?: number | undefined
  descriptorSeen: boolean
  lastTurnCompleted?: boolean | undefined
}

export const initTimingState = (): SubagentTimingState => ({ descriptorSeen: false, settledMs: 0 })

/** 一条子 journal 事件对 timing 的折叠（eventType/reasonKind 取本层 push 时的已知值）。 */
export function foldTimingEvent(
  state: SubagentTimingState,
  eventType: string,
  at: number,
  reasonKind?: string,
): SubagentTimingState {
  if (eventType === 'turn/start') {
    const { lastTurnCompleted: _closed, ...openState } = state
    return state.descriptorSeen
      ? { ...openState, active: { since: at, through: at } }
      : { ...openState, pendingTurnStart: at }
  }
  if (eventType === 'subagent/descriptor') {
    const activeSince = state.active?.since ?? state.pendingTurnStart
    return {
      descriptorSeen: true,
      settledMs: 0,
      ...(activeSince === undefined ? {} : { active: { since: activeSince, through: at } }),
    }
  }
  if (eventType === 'turn/end') {
    if (!state.descriptorSeen) {
      if (state.pendingTurnStart === undefined) return state
      const { pendingTurnStart: _closed, ...next } = state
      return next
    }
    if (state.active === undefined) return state
    const { active: _active, ...rest } = state
    return {
      ...rest,
      settledMs: state.settledMs + Math.max(0, at - state.active.since),
      lastTurnCompleted: reasonKind === 'completed',
    }
  }
  if (state.active === undefined) return state
  return { ...state, active: { ...state.active, through: at } }
}

/** 折叠状态 → 投影值（undefined 字段不挂键，与上游 wire view 同形）。 */
export function timingView(state: SubagentTimingState): SubagentTimingProjection {
  return {
    settledMs: state.settledMs,
    ...(state.active === undefined ? {} : { active: state.active }),
    ...(state.lastTurnCompleted === undefined ? {} : { lastTurnCompleted: state.lastTurnCompleted }),
  }
}

/** 泳道投影值的组装：timing 只在见过 turn 后挂（无 descriptor 时 settledMs 恒 0，UI 按 active 判读）。 */
export function timingProjectionOf(state: SubagentTimingState | undefined): SubagentTimingProjection | undefined {
  if (state === undefined) return undefined
  if (state.settledMs === 0 && state.active === undefined && !state.descriptorSeen) return undefined
  return timingView(state)
}
