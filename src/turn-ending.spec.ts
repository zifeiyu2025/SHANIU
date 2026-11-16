/**
 * 一个回合只允许一种结局口径（真机冒烟 2026-11-17 N3）。
 *
 * 现象：截断回合同屏三种说法——行首红字「本轮运行失败」、正文「回答被截断」、
 * 回合组标题「已完成，用时 5 秒」。
 * 根因：`turn-error`、`turn-max-tokens`、`TurnProcessNodeView` 各自读一次
 * `reason.kind` 再各判一次；截断在其中两处分别落进「失败」与默认的「已完成」分支。
 *
 * 用例放在仓库自己的测试集里（根 vitest 排除 `src/dsh`，vendored 用例不会跑）。
 */
import { describe, expect, it } from 'vitest'

import type { TurnEndReason } from '@deepseek-ai/dsh-session/types'
import { turnMaxTokensDefinition } from './dsh/packages/client/ui-chat/src/client/conversation-nodes/turn-max-tokens'
import { turnErrorDefinition } from './dsh/packages/client/ui-chat/src/client/conversation-nodes/turn-error'
import { turnEnding, turnEndingRow } from './dsh/packages/client/ui-chat/src/client/conversation-nodes/turn-ending'
import { turnProcessOutcome } from './dsh/packages/client/ui-chat/src/client/chat/TurnProcessNodeView'
import { zh } from './dsh/packages/client/ui-chat/src/client/locale'

/** 真机上「用时 5 秒」的那段时长；只要落进 took 就会被原样显示。 */
const FIVE_SECONDS_MS = 5_000

type MatchInput = Parameters<typeof turnErrorDefinition['match']>[0]

function turnEnd(turn: number, reason: TurnEndReason): MatchInput {
  return {
    type: 'turn/end',
    seq: 42,
    time: 1_700_000_000_000,
    data: { turn, reason },
  } as unknown as MatchInput
}

describe('回合结局：截断只算「被截断」，不算失败也不算已完成', () => {
  it('单一判定把 max-tokens 判成 truncated', () => {
    expect(turnEnding({ kind: 'max-tokens' })).toEqual({ kind: 'truncated', code: 'MAX_TOKENS' })
  })

  it('行首：截断不再生成「本轮运行失败」结局行，只留截断提示行', () => {
    const event = turnEnd(1, { kind: 'max-tokens' })
    // 反向对照：把 `turn-error` 的 max-tokens 分支加回去，这里会变成 { id: '1', role: 'update' }。
    expect(turnErrorDefinition.match(event)).toBeNull()
    expect(turnEndingRow(turnEnding({ kind: 'max-tokens' }))).toBeUndefined()
    // 「被截断」仍有一行承载，且与上面是同一个判定。
    expect(turnMaxTokensDefinition.match(event)).toEqual({ id: '1', role: 'start' })
  })

  it('组标题：截断显示「已截断」，不带「已完成」也不带用时', () => {
    const ending = turnEnding({ kind: 'max-tokens' })
    // 反向对照：删掉 truncated 分支，这里会变成 { kind: 'took', elapsedMs: 5000 }。
    expect(turnProcessOutcome(ending.kind, FIVE_SECONDS_MS)).toEqual({ kind: 'truncated' })
    expect(zh['message.turnProcess.truncated']).toBe('已截断')
    expect(zh['message.turnProcess.truncated']).not.toBe(zh['message.turnProcess.worked'])
    expect(zh['message.turnProcess.truncated']).not.toBe(zh['message.turnProcess.failed'])
  })

  it('其余结局的口径不变', () => {
    expect(turnEnding(undefined)).toEqual({ kind: 'completed' })
    expect(turnEnding({ kind: 'completed' })).toEqual({ kind: 'completed' })
    expect(turnEnding({ kind: 'aborted', reason: { kind: 'user' } })).toEqual({ kind: 'stopped' })
    expect(turnEnding({ kind: 'interrupted' }).kind).toBe('stopped')
    expect(turnEnding({ kind: 'indeterminate' }).kind).toBe('unknown')
    expect(turnEnding({ kind: 'error', error: { message: '风控拦截', code: 'RATE_LIMITED' } }).kind)
      .toBe('failed')

    const failed = turnEnd(2, { kind: 'error', error: { message: '风控拦截', code: 'RATE_LIMITED' } })
    expect(turnErrorDefinition.match(failed)).toEqual({ id: '2', role: 'update' })
    expect(turnMaxTokensDefinition.match(failed)).toBeNull()
  })
})
