/**
 * 回合顶部摘要（「已完成，用时 N」那一行）与档案结论必须同一口径。
 *
 * 真机现象：旧档案没有 `turn_end` 行，回合内已经如实显示「本轮状态未知」，
 * 顶部却显示「已完成，用时 17 小时」——同一个回合两处结论相反。
 * 根因在 vendored 的 `TurnProcessNodeView`：它只特判 `aborted` / `interrupted` / `error`，
 * 重建补的 `indeterminate` 落进默认分支，被当成 completed 计时。
 *
 * 2026-11-17 冒烟 N3 之后，顶部不再自己读 `reason.kind`：结局由 `turnEnding`
 * 统一判定，这里只验证「结局 → 展示标签」的映射。
 *
 * 用例放在仓库自己的测试集里（根 vitest 排除 `src/dsh`，vendored 用例不会跑）。
 */
import { describe, expect, it } from 'vitest'

import { turnEnding } from './dsh/packages/client/ui-chat/src/client/conversation-nodes/turn-ending'
import { turnProcessOutcome } from './dsh/packages/client/ui-chat/src/client/chat/TurnProcessNodeView'
import { zh } from './dsh/packages/client/ui-chat/src/client/locale'

/** 真机上「17 小时」的那段时长；只要落进 took 就会被原样显示。 */
const SEVENTEEN_HOURS_MS = 17 * 60 * 60 * 1000

describe('回合顶部摘要：没有结论不得写成已完成', () => {
  it('indeterminate → 本轮状态未知，且不给用时', () => {
    // 反向对照：删掉 `unknown` 分支，这里会变成 { kind: 'took', elapsedMs: ... }，
    // 界面随即显示「已完成，用时 17小时」，与回合内的「本轮状态未知」互相矛盾。
    expect(turnProcessOutcome(turnEnding({ kind: 'indeterminate' }).kind, SEVENTEEN_HOURS_MS))
      .toEqual({ kind: 'unknown' })
    expect(zh['message.turnUnknown']).toBe('本轮状态未知')
  })

  it('completed 保留用时，与旧行为一致', () => {
    expect(turnProcessOutcome(turnEnding({ kind: 'completed' }).kind, SEVENTEEN_HOURS_MS))
      .toEqual({ kind: 'took', elapsedMs: SEVENTEEN_HOURS_MS })
  })

  it('已中止 / 进程退出 / 失败仍然替换用时', () => {
    expect(turnProcessOutcome(turnEnding({ kind: 'aborted', reason: { kind: 'user' } }).kind, SEVENTEEN_HOURS_MS))
      .toEqual({ kind: 'stopped' })
    expect(turnProcessOutcome(turnEnding({ kind: 'interrupted' }).kind, SEVENTEEN_HOURS_MS))
      .toEqual({ kind: 'stopped' })
    expect(turnProcessOutcome(
      turnEnding({ kind: 'error', error: { message: '风控拦截', code: 'RATE_LIMITED' } }).kind,
      SEVENTEEN_HOURS_MS,
    )).toEqual({ kind: 'failed' })
  })

  it('既无结局也无时长时退回「已完成」，不新造文案', () => {
    expect(turnProcessOutcome(turnEnding(undefined).kind, undefined)).toEqual({ kind: 'worked' })
  })
})
