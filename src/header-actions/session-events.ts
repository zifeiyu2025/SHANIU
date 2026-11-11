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
 * ① `seq` 是本地合成的单调值，不是真实日志序号（formatter 不读，仅用于满足类型）；
 * ② `assistant/message` 的 `source`（provider/model 溯源）在 SHANIU 档案里没有等价物，
 *    填最小合法形状——formatter 只读 `content`，读不到这部分；
 * ③ `step` 恒为 1：档案不记步号，而 formatter 的分段只用 turn，不用 step。
 *
 * **时间（A0-4）**：`time` 一律取档案里的事件落档时刻 `at_ms`，不再用导出那一刻
 * （那会让整份导出只有一两个不同时间）。旧档案没有 `at_ms`：写 `time: 0` 并附
 * `timeUnreliable: true`，明说自己不可信，绝不拿当前时钟冒充。
 */
import type { SessionEvent } from '@deepseek-ai/dsh-session/types'
import type { SessionTitleEventData } from '@deepseek-ai/dsh-session-title/types'
import type { TurnErrorInfo } from '../transport/types'
import { archiveImageToPrompt, type SessionImageRefWire } from '../transport/image-ref'

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
  | {
    readonly kind: 'message'
    readonly role: string
    readonly text: string
    readonly reasoning?: string
    /**
     * 用户消息附带的图片引用（`src-tauri/src/lib.rs` 的 `Message.images`）。
     * 导出/复制读它，而不是把图片压成正文里的 `[图片: …]` 文本——上游 formatter
     * 本来就认 `image` 块（`markdown.ts` 的 `visibleContent`）。
     */
    readonly images?: readonly SessionImageRefWire[]
    /** 事件落档时刻（Unix 毫秒）；缺省或 0 = 旧档案未记录（A0-4）。 */
    readonly at_ms?: number
  }
  | {
    readonly kind: 'tool_call'
    readonly call_id: string
    readonly tool: string
    readonly arguments: string
    readonly output?: string
    /** 事件落档时刻；缺省或 0 = 旧档案未记录（A0-4）。 */
    readonly at_ms?: number
  }
  | {
    /** 回合结局（A0-3）：`completed` / `cancelled` / `failed` / `budget_exhausted`。 */
    readonly kind: 'turn_end'
    readonly turn: number
    readonly reason: string
    readonly at_ms: number
    readonly error?: TurnErrorInfo
  }

/** 一条档案条目的时间：有落档时刻就用它，否则如实标不可信（不拿导出时刻冒充）。 */
function entryTime(entry: { readonly at_ms?: number }): { time: number; unreliable: boolean } {
  const at = entry.at_ms
  return typeof at === 'number' && at > 0
    ? { time: at, unreliable: false }
    : { time: 0, unreliable: true }
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

  /** 推入一条事件：`time` 取档案时刻，未知则 0 + `timeUnreliable`（只加不改已有字段）。 */
  const push = (event: Record<string, unknown>, at: { time: number; unreliable: boolean }): void => {
    seq += 1
    events.push({
      ...event,
      time: at.time,
      ...at.unreliable ? { timeUnreliable: true } : {},
      seq,
    } as Mutable<SessionEvent>)
  }

  // 标题事件放在最前：formatter 用 `findLast(event => type === 'session/title')`，
  // 只放一条即可；放最前面也符合"标题先于正文"的阅读顺序。标题本身没有发生时刻，
  // 取档案里第一条可靠时间；整份档案都没有就标不可信。
  const firstKnown = entries.find(entry => 'at_ms' in entry && (entry.at_ms ?? 0) > 0)
  push({ type: 'session/title', data: { title } }, firstKnown === undefined
    ? { time: 0, unreliable: true }
    : entryTime(firstKnown))

  /**
   * 收口当前回合。
   *
   * `reason` 缺省为 `unknown`：旧档案没有结局行，只能如实说"不知道"。
   * 写成 `completed` 会让失败回合在导出里变成成功，任何基于档案的统计、
   * 审计、评测都会被骗（A0-3）。
   */
  const closeTurn = (
    reason = 'unknown',
    at?: { time: number; unreliable: boolean },
    error?: TurnErrorInfo,
  ): void => {
    if (!open) return
    push({
      type: 'turn/end',
      data: error === undefined ? { turn, reason } : { turn, reason, error },
    }, at ?? { time: 0, unreliable: true })
    open = false
  }

  for (const entry of entries) {
    // 结局行由宿主在回合收口时写入；它不属于新回合，只负责关闭当前回合。
    if (entry.kind === 'turn_end') {
      if (entry.turn === turn) closeTurn(entry.reason, entryTime(entry), entry.error)
      continue
    }
    if (entry.kind === 'message') {
      if (entry.role === 'user') {
        closeTurn()
        turn += 1
        const at = entryTime(entry)
        push({ type: 'turn/start', data: { turn } }, at)
        open = true
        push({
          type: 'user/message',
          surfaceOp: 'append',
          data: {
            id: `shaniu-user-${String(seq)}`,
            role: 'user',
            content: [
              { type: 'text', text: entry.text },
              // 附件不是正文的一部分：图片按 `image` 块交给 formatter，否则导出/复制
              // 会把一次带图提问写成一个没有图片的纯文本气泡。
              ...(entry.images ?? []).flatMap((image) => {
                const attachment = archiveImageToPrompt(image)
                return attachment === undefined ? [] : [{ type: 'image' as const, attachment }]
              }),
            ],
            source: { kind: 'user' },
          },
        }, at)
        continue
      }
      if (entry.role !== 'assistant') continue // system / developer 不在 formatter 的渲染面内
      push({
        type: 'assistant/message',
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
      }, entryTime(entry))
      continue
    }

    // tool_call：调用与结果配对展开。`callId` 用档案里的 call_id，
    // formatter 靠它把 result 回填到同一条 ToolEntry 上。
    const at = entryTime(entry)
    push({
      type: 'tool/call',
      data: { turn, step: 1, callId: entry.call_id, name: entry.tool, arguments: entry.arguments },
    }, at)
    if (entry.output !== undefined) {
      push({
        type: 'tool/result',
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
      }, at)
    }
  }

  closeTurn()
  return events as unknown as SessionEvent[]
}
