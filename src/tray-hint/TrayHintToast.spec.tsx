import { act, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const { listeners } = vi.hoisted(() => ({
  listeners: [] as ((event: { payload: { count: number } }) => void)[],
}))

vi.mock('@tauri-apps/api/event', () => ({
  listen: async (_name: string, handler: (event: { payload: { count: number } }) => void) => {
    listeners.push(handler)
    return () => {}
  },
}))

import { TrayHintToast } from './TrayHintToast'

/** 模拟宿主发一次提示事件。 */
function emit(count: number): void {
  act(() => { for (const handler of listeners) handler({ payload: { count } }) })
}

describe('TrayHintToast', () => {
  beforeEach(() => {
    listeners.length = 0
    vi.useFakeTimers()
  })

  afterEach(() => {
    vi.useRealTimers()
  })

  it('收到存活会话数后显示，并说明退出应用才会终止', async () => {
    render(<TrayHintToast />)
    await act(async () => {})
    expect(screen.queryByRole('alert')).toBeNull()

    emit(2)
    const alert = screen.getByRole('alert')
    expect(alert.textContent).toContain('仍有 2 个终端会话在运行')
    expect(alert.textContent).toContain('退出应用才会终止')
  })

  it('淡出结束后自行卸载，不留常驻提示', async () => {
    render(<TrayHintToast />)
    await act(async () => {})
    emit(1)
    expect(screen.queryByRole('alert')).not.toBeNull()

    act(() => { vi.advanceTimersByTime(6000 + 1000) })
    expect(screen.queryByRole('alert')).toBeNull()
  })

  it('计数为 0 时不提示', async () => {
    render(<TrayHintToast />)
    await act(async () => {})
    emit(0)
    expect(screen.queryByRole('alert')).toBeNull()
  })
})
