/**
 * 消息时钟的「时刻不可用」分支（A0-4）。
 *
 * 背景：`formatMessageClock` 来自 vendored DSH，上游对 `time = 0` 会渲染成 1970 年。
 * 旧档案没有事件时刻，显示假日期比不显示更糟——这里锁住"0 必须落到词典键"的行为。
 * 放到仓库自己的测试集里（根 vitest 排除 `src/dsh`，vendored 用例不会跑）。
 */
import { describe, expect, it } from 'vitest'

import { formatMessageClock, type ClockTranslate } from './dsh/packages/client/ui-chat/src/client/chat/message-chrome'
import { zh } from './dsh/packages/client/ui-chat/src/client/locale'

const t = ((key: string) => (zh as Record<string, string>)[key] ?? key) as unknown as ClockTranslate

describe('消息时钟：档案未记录时刻', () => {
  const now = new Date(2026, 6, 29, 10, 0).getTime()

  it('time <= 0 显示"记录时刻不可用"，不显示 1970', () => {
    // 反向对照：撤掉 `time <= 0` 分支，这里会变成 "1970年1月1日 08:00"。
    expect(formatMessageClock(0, t, now)).toBe('记录时刻不可用')
    expect(formatMessageClock(-1, t, now)).toBe('记录时刻不可用')
  })

  it('有真实时刻时与上游行为一致（当天只显示 HH:mm）', () => {
    expect(formatMessageClock(new Date(2026, 6, 29, 14, 24).getTime(), t, now)).toBe('14:24')
  })
})
