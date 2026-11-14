import { memo } from 'react'
import { IconChevronDownOutlineRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import type { ChatNodeViewProps } from '../contract/slots.ts'
import { turnProcessAlwaysOpen } from '../contract/turn-process.ts'
import { formatRunDuration } from './message-chrome.ts'
import a11yCss from './accessibility.module.css'
import css from './TurnProcessNodeView.module.css'

/** What the settled Turn control says about how the Turn ended. */
export type TurnProcessOutcome =
  | { readonly kind: 'stopped' }
  | { readonly kind: 'failed' }
  | { readonly kind: 'unknown' }
  | { readonly kind: 'worked' }
  | { readonly kind: 'took'; readonly elapsedMs: number }

/**
 * Map a Turn's end reason to its control label.
 *
 * `indeterminate` is SHANIU's "no recorded ending" reason (see `turn-error.ts`).
 * It must not fall through to the completed branch: that renders
 * 「已完成，用时 N」for a Turn the archive never concluded, which contradicts
 * the「本轮状态未知」row inside the same Turn. Unknown means unknown — no
 * duration either, because a duration would again read as "it finished".
 */
export function turnProcessOutcome(
  reasonKind: string | undefined,
  elapsedMs: number | undefined,
): TurnProcessOutcome {
  if (reasonKind === 'aborted' || reasonKind === 'interrupted') return { kind: 'stopped' }
  if (reasonKind === 'error') return { kind: 'failed' }
  if (reasonKind === 'indeterminate') return { kind: 'unknown' }
  return elapsedMs === undefined ? { kind: 'worked' } : { kind: 'took', elapsedMs }
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
  const reason = turn.end?.data.reason.kind
  const elapsedMs = turn.start === undefined || turn.end === undefined ? undefined
    : Math.max(1000, turn.end.time - turn.start.time)
  const outcome = turnProcessOutcome(reason, elapsedMs)
  const duration = outcome.kind === 'took' ? formatRunDuration(outcome.elapsedMs, t) : undefined
  // Stops, failures and unknown endings replace elapsed time; the rest keep it.
  const label = outcome.kind === 'stopped' ? t('message.stopped')
    : outcome.kind === 'failed' ? t('message.turnProcess.failed')
      : outcome.kind === 'unknown' ? t('message.turnUnknown')
        : outcome.kind === 'took' ? t('message.turnProcess.took')
          : t('message.turnProcess.worked')
  const announcement = outcome.kind === 'stopped' ? t('message.stopped')
    : outcome.kind === 'failed' ? t('message.turnProcess.failed')
      : outcome.kind === 'unknown' ? t('message.turnUnknown')
        : t('message.turnProcess.worked')
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
