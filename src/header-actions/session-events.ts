/**
 * 档案条目 → DSH 逻辑事件序列（SH-114 批 1 的适配层）。
 *
 * **为什么需要这一步**：上游 `formatSessionMarkdown(events: SessionEvent[])` 吃的是
 * 「完整 committed 逻辑事件快照」，而 SHANIU 宿主 `load_session` 返回的是
 * `SessionEntry[]`（展示投影：`message` / `tool_call` 两种）。两者都是"同一份档案的
 * 推导结果"，所以这里做一次形状翻译，而不是在 Rust 侧再造一套事件类型——
 * 后者会让"导出 / 回放 / 搜索"三处各有一份事件推导，迟早互相漂移（SH-108 立的口径）。
 *
 * **映射规则（逐条声明，因为它决定了 Markdown 的分段）**
 * - 每条 `role=user` 的 message 开一个新 turn：上游 `collectTurns` 靠 `turn/start`
 *   切回合，而 SHANIU 档案里没有 turn 边界标记；一个用户任务 = 一个回合，这是同一件事。
 * - `tool_call` 展开成 `tool/call` + 紧邻的 `tool/result`。档案里输出与调用是配对的
 *   （`output` 可选），上游 formatter 用 `callId` 回填，形状对得上。
 * - `reasoning` 不进事件：上游 formatter 明确省略 reasoning（`assistantText` 只读
 *   `content` 的 text/image/file 块），塞进去也不会被渲染，反而会让快照与真实日志更像
 *   却又不是——不如不塞，在偏差里写清。
 *
 * **已知偏差**（不改上游 formatter 的代价，逐条记在 docs/05）：
 * ① `seq` / `time` 是本地合成的单调值，不是真实日志序号（formatter 不读，仅用于满足类型）；
 * ② `assistant/message` 的 `source`（provider/model 溯源）在 SHANIU 档案里没有等价物，
 *    填最小合法形状——formatter 只读 `content`，读不到这部分；
 * ③ `step` 恒为 1：档案不记步号，而 formatter 的分段只用 turn，不用 step。
 */
import type { SessionEvent } from '@deepseek-ai/dsh-session/types'
import type { SessionTitleEventData } from '@deepseek-ai/dsh-session-title/types'
import type { TurnErrorInfo } from '../transport/types'

/**
 * `session/title` 事件的类型增强。
 *
 * 上游把这段 `declare module` 写在 `session-title/src/index.ts`——但那是 **host 插件**
 * （依赖 `ISessions.get` 等宿主 API），client 类型上下文里整体引入会把整包拖进来并报错。
 * 这里只取它声明的那**一个键**，数据源仍是上游 `types.ts` 的 `SessionTitleEventData`，
 * 形状与上游 `index.ts:71-79` 一致；上游改形状时这里跟着改，**不要在此另立字段**。
 */
declare module '@deepseek-ai/dsh-session/types' {
  interface SessionEventMap {
    'session/title': SessionTitleEventData
  }
}

/** 宿主 `load_session` 返回的一条历史（与 `src-tauri/src/lib.rs` 的 `SessionEntry` 对应）。 */
export type SessionEntryLike =
  | { readonly kind: 'message'; readonly role: string; readonly text: string; readonly reasoning?: string }
  | {
    readonly kind: 'tool_call'
    readonly call_id: string
    readonly tool: string
    readonly arguments: string
    readonly output?: string
  }
  | {
    /** 回合结局（A0-3）：`completed` / `cancelled` / `failed` / `budget_exhausted`。 */
    readonly kind: 'turn_end'
    readonly turn: number
    readonly reason: string
    readonly at_ms: number
    readonly error?: TurnErrorInfo
  }

/** 合成事件的载体：`SessionEvent` 是带条件字段的判别联合，逐字段构造过不了收窄，这里统一承载。 */
type Mutable<T> = { -readonly [K in keyof T]: T[K] }

/**
 * 把展示条目翻译成上游 Markdown formatter 期望的事件序列。
 *
 * @param entries - `load_session` 的完整返回（整份档案，不依赖界面已加载多少）。
 * @param title - 会话标题；作为 `session/title` 事件，formatter 用它做一级标题。
 * @returns 按 seq 升序的事件数组，末尾一定有 `turn/end`（若存在未闭合回合）。
 */
export function sessionEntriesToEvents(
  entries: readonly SessionEntryLike[],
  title: string,
): SessionEvent[] {
  const events: Mutable<SessionEvent>[] = []
  let seq = 0
  let turn = 0
  let open = false

  const push = (event: Record<string, unknown>): void => {
    seq += 1
    events.push({ ...event, seq } as Mutable<SessionEvent>)
  }

  // 标题事件放在最前：formatter 用 `findLast(event => type === 'session/title')`，
  // 只放一条即可；放最前面也符合"标题先于正文"的阅读顺序。
  push({ type: 'session/title', time: Date.now(), data: { title } })

  /**
   * 收口当前回合。
   *
   * `reason` 缺省为 `unknown`：旧档案没有结局行，只能如实说"不知道"。
   * 写成 `completed` 会让失败回合在导出里变成成功，任何基于档案的统计、
   * 审计、评测都会被骗（A0-3）。
   */
  const closeTurn = (
    reason = 'unknown',
    atMs?: number,
    error?: TurnErrorInfo,
  ): void => {
    if (!open) return
    push({
      type: 'turn/end',
      time: atMs ?? Date.now(),
      data: error === undefined ? { turn, reason } : { turn, reason, error },
    })
    open = false
  }

  for (const entry of entries) {
    // 结局行由宿主在回合收口时写入；它不属于新回合，只负责关闭当前回合。
    if (entry.kind === 'turn_end') {
      if (entry.turn === turn) closeTurn(entry.reason, entry.at_ms, entry.error)
      continue
    }
    if (entry.kind === 'message') {
      if (entry.role === 'user') {
        closeTurn()
        turn += 1
        push({ type: 'turn/start', time: Date.now(), data: { turn } })
        open = true
        push({
          type: 'user/message',
          time: Date.now(),
          surfaceOp: 'append',
          data: {
            id: `shaniu-user-${String(seq)}`,
            role: 'user',
            content: [{ type: 'text', text: entry.text }],
            source: { kind: 'user' },
          },
        })
        continue
      }
      if (entry.role !== 'assistant') continue // system / developer 不在 formatter 的渲染面内
      push({
        type: 'assistant/message',
        time: Date.now(),
        surfaceOp: 'append',
        data: {
          turn,
          step: 1,
          message: {
            id: `shaniu-assistant-${String(seq)}`,
            role: 'assistant',
            content: [{ type: 'text', text: entry.text }],
            source: { kind: 'model' },
          },
          stream: [],
        },
      })
      continue
    }

    // tool_call：调用与结果配对展开。`callId` 用档案里的 call_id，
    // formatter 靠它把 result 回填到同一条 ToolEntry 上。
    push({
      type: 'tool/call',
      time: Date.now(),
      data: { turn, step: 1, callId: entry.call_id, name: entry.tool, arguments: entry.arguments },
    })
    if (entry.output !== undefined) {
      push({
        type: 'tool/result',
        time: Date.now(),
        surfaceOp: 'append',
        data: {
          turn,
          step: 1,
          message: {
            id: `shaniu-tool-${entry.call_id}`,
            role: 'tool',
            content: [{ type: 'text', text: entry.output }],
            source: { kind: 'tool', callId: entry.call_id },
            toolCallId: entry.call_id,
          },
        },
      })
    }
  }

  closeTurn()
  return events as unknown as SessionEvent[]
}
