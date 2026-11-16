import { memo } from 'react'
import { IconChevronDownOutlineRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import type { ChatNodeViewProps } from '../contract/slots.ts'
import { turnProcessAlwaysOpen } from '../contract/turn-process.ts'
import type { TurnEndingKind } from '../conversation-nodes/turn-ending.ts'
import { turnEnding } from '../conversation-nodes/turn-ending.ts'
import { formatRunDuration } from './message-chrome.ts'
import a11yCss from './accessibility.module.css'
import css from './TurnProcessNodeView.module.css'

/** What the settled Turn control says about how the Turn ended. */
export type TurnProcessOutcome =
  | { readonly kind: 'stopped' }
  | { readonly kind: 'failed' }
  | { readonly kind: 'unknown' }
  | { readonly kind: 'truncated' }
  | { readonly kind: 'worked' }
  | { readonly kind: 'took'; readonly elapsedMs: number }

/**
 * Map a Turn's ending to its control label.
 *
 * The ending comes from `turnEnding` — the single judgement shared with the row
 * inside the Turn. Re-judging `reason.kind` here is what produced the 2026-11-17
 * smoke contradiction:「本轮运行失败」row and「已完成，用时 5 秒」header for the
 * same truncated Turn. `truncated` and `unknown` carry no duration: a duration
 * would read as "it finished".
 */
export function turnProcessOutcome(
  ending: TurnEndingKind,
  elapsedMs: number | undefined,
): TurnProcessOutcome {
  if (ending === 'stopped') return { kind: 'stopped' }
  if (ending === 'failed') return { kind: 'failed' }
  if (ending === 'unknown') return { kind: 'unknown' }
  if (ending === 'truncated') return { kind: 'truncated' }
  return elapsedMs === undefined ? { kind: 'worked' } : { kind: 'took', elapsedMs }
}

/** 结局 → 组标题词典键；截断/失败/未知各有自己的说法，不借用「已完成」。 */
type TurnProcessLabelKey =
  | 'message.stopped'
  | 'message.turnProcess.failed'
  | 'message.turnUnknown'
  | 'message.turnProcess.truncated'
  | 'message.turnProcess.took'
  | 'message.turnProcess.worked'

function outcomeLabelKey(outcome: TurnProcessOutcome): TurnProcessLabelKey {
  switch (outcome.kind) {
    case 'stopped': return 'message.stopped'
    case 'failed': return 'message.turnProcess.failed'
    case 'unknown': return 'message.turnUnknown'
    case 'truncated': return 'message.turnProcess.truncated'
    case 'took': return 'message.turnProcess.took'
    case 'worked': return 'message.turnProcess.worked'
  }
}

/** Settled Turn duration and process disclosure above its content. */
export const TurnProcessNodeView = memo(function TurnProcessNodeView({
  node, turnProcess, t,
}: ChatNodeViewProps<'turn-process'>) {
  if (turnProcess === undefined) throw new Error('turn-process node requires Turn process owner state')
  const open = !turnProcess.foldable || turnProcess.open
  const turn = node.location.kind === 'turn' || node.location.kind === 'step'
    ? node.location.turn
    : undefined
  if (turn?.status !== 'closed') return null
  const canCollapse = turnProcess.foldable && turnProcess.hasContent && !turnProcessAlwaysOpen(node)
  // 结局只问 `turnEnding`（与回合内的结局行同一判定）；这里只做展示映射。
  const ending = turn.end === undefined ? 'completed' : turnEnding(turn.end.data.reason).kind
  const elapsedMs = turn.start === undefined || turn.end === undefined ? undefined
    : Math.max(1000, turn.end.time - turn.start.time)
  const outcome = turnProcessOutcome(ending, elapsedMs)
  const duration = outcome.kind === 'took' ? formatRunDuration(outcome.elapsedMs, t) : undefined
  // Stops, failures, unknown and truncated endings replace elapsed time; the rest keep it.
  const label = t(outcomeLabelKey(outcome))
  // 屏幕阅读器播报的是「结束了什么」；带用时的完成沿用旧的「已完成」播报。
  const announcement = t(outcome.kind === 'took' ? 'message.turnProcess.worked' : outcomeLabelKey(outcome))
  return (
    <>
      <span className={a11yCss.visuallyHidden} role="status" aria-live="polite" aria-atomic="true">{announcement}</span>
      <button
        type="button"
        className={css.root}
        data-open={open || undefined}
        data-turn-process={node.data.turn}
        data-turn-process-messages={node.data.messageCount}
        data-turn-process-tool-calls={node.data.toolCallCount}
        data-turn-process-subagents={node.data.subagentCount}
        disabled={!canCollapse}
        aria-expanded={turnProcess.hasContent ? open : undefined}
        onClick={(event) => {
          event.currentTarget.focus()
          turnProcess.setOpen(!open)
        }}
      >
        <span className={css.label}>
          {label}
          {duration?.map((part, index) => (
            <span key={index} className={part.numeric ? css.durationNumber : undefined}>{part.text}</span>
          ))}
        </span>
        {canCollapse && <IconChevronDownOutlineRegular className={css.chevron} />}
      </button>
    </>
  )
})
