import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

interface Payload { count: number; first?: boolean }

const { listeners } = vi.hoisted(() => ({
  listeners: [] as ((event: { payload: Payload }) => void)[],
}))

vi.mock('@tauri-apps/api/event', () => ({
  listen: async (_name: string, handler: (event: { payload: Payload }) => void) => {
    listeners.push(handler)
    return () => {}
  },
}))

import { TrayHintToast } from './TrayHintToast'
import { zh, type TrayHintKey } from './locales'

/** 词典桩：只验插值与渲染契约，文案本身以 `locales.ts` 为准。 */
const t = ((key: TrayHintKey, params?: { count?: number }) =>
  zh[key].replace('{count}', String(params?.count ?? 0))) as never

/** 模拟宿主发一次提示事件。 */
function emit(payload: Payload): void {
  act(() => { for (const handler of listeners) handler({ payload }) })
}

describe('TrayHintToast', () => {
  beforeEach(() => {
    listeners.length = 0
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('有任务时说明还有几个、以及怎么真正退出', async () => {
    render(<TrayHintToast t={t} />)
    await act(async () => {})
    expect(screen.queryByRole('alert')).toBeNull()

    emit({ count: 2 })
    const alert = screen.getByRole('alert')
    expect(alert.textContent).toContain('2 个终端/任务未结束')
    expect(alert.textContent).toContain('右键托盘图标')
    expect(alert.textContent).toContain('退出 SHANIU')
  })

  it('没有任务时也必须提示“程序仍在运行”，不能静默隐藏', async () => {
    render(<TrayHintToast t={t} />)
    await act(async () => {})
    emit({ count: 0 })
    const alert = screen.getByRole('alert')
    expect(alert.textContent).toContain('程序仍在运行')
    expect(alert.textContent).toContain('右键托盘图标')
  })

  it('首次关闭补一句托盘图标位置，之后不再重复', async () => {
    render(<TrayHintToast t={t} />)
    await act(async () => {})
    emit({ count: 0, first: true })
    expect(screen.getByRole('alert').textContent).toContain('可能收在「^」里')

    emit({ count: 0, first: false })
    expect(screen.getByRole('alert').textContent).not.toContain('可能收在「^」里')
  })

  it('淡出结束后自行卸载，不留常驻提示', async () => {
    render(<TrayHintToast t={t} />)
    await act(async () => {})
    emit({ count: 1 })
    expect(screen.queryByRole('alert')).not.toBeNull()

    act(() => { vi.advanceTimersByTime(8000 + 1000) })
    expect(screen.queryByRole('alert')).toBeNull()
  })
})
