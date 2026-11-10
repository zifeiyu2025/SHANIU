import { act, fireEvent, render, screen } from '@testing-library/react'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

const { listeners, invoke } = vi.hoisted(() => ({
  listeners: [] as ((event: { payload: unknown }) => void)[],
  invoke: vi.fn(async (_command: string, _args?: Record<string, unknown>) => undefined as unknown),
}))

vi.mock('@tauri-apps/api/event', () => ({
  listen: async (_name: string, handler: (event: { payload: unknown }) => void) => {
    listeners.push(handler)
    return () => {}
  },
}))

vi.mock('../transport/tauri', () => ({
  tauriBridge: () => ({ invoke }),
}))

import { QuitDialog } from './QuitDialog'
import { QUIT_DIALOG_EVENT, requestQuit, type QuitSurvey } from './quit'
import { zh, type QuitGuardKey } from './locales'

const t = ((key: QuitGuardKey, params?: Record<string, unknown>) =>
  Object.entries(params ?? {}).reduce(
    (text, [name, value]) => text.replaceAll(`{${name}}`, String(value)),
    zh[key] as string,
  )) as never

const titleOf = (id: string): string => ({ 'sess-1': '在线检查', 'sess-2': '发布准备' } as Record<string, string>)[id] ?? id

const idleSurvey: QuitSurvey = {
  runningJobs: [], runningTerminals: 0, pendingApprovals: 0, busy: false,
}

const busySurvey: QuitSurvey = {
  activeSession: 'sess-1',
  runningJobs: [{ id: 'job-1', sessionId: 'sess-2', label: 'npm test', status: 'running' }],
  runningTerminals: 1,
  pendingApprovals: 2,
  busy: true,
}

function emitTray(survey: QuitSurvey): void {
  act(() => { for (const handler of listeners) handler({ payload: survey }) })
}

function renderDialog(overrides: Partial<{ cancelAndQuit: () => Promise<void>; hideToTray: () => Promise<void> }> = {}) {
  const cancelAndQuit = overrides.cancelAndQuit ?? vi.fn(async () => {})
  const hideToTray = overrides.hideToTray ?? vi.fn(async () => {})
  render(
    <QuitDialog
      t={t}
      titleOf={titleOf}
      cancelAndQuit={cancelAndQuit}
      hideToTray={hideToTray}
    />,
  )
  return { cancelAndQuit, hideToTray }
}

describe('QuitDialog', () => {
  beforeEach(() => {
    listeners.length = 0
    invoke.mockClear()
  })

  afterEach(() => {
    vi.restoreAllMocks()
  })

  it('没有任务时不弹窗（宿主会直接退出）', async () => {
    renderDialog()
    await act(async () => {})
    emitTray(idleSurvey)
    expect(screen.queryByRole('dialog')).toBeNull()
  })

  it('有任务时说明在哪些会话、几个作业/终端/待审批', async () => {
    renderDialog()
    await act(async () => {})
    emitTray(busySurvey)
    const dialog = screen.getByRole('dialog')
    expect(dialog.textContent).toContain('正在运行的会话：在线检查')
    expect(dialog.textContent).toContain('后台作业：1 个（发布准备）')
    expect(dialog.textContent).toContain('后台终端：1 个')
    expect(dialog.textContent).toContain('待审批：2 项')
  })

  it('三个选项都在，且取消并退出调用取消链', async () => {
    let resolveCancel: (() => void) | undefined
    const cancelAndQuit = vi.fn(() => new Promise<void>(resolve => { resolveCancel = resolve }))
    renderDialog({ cancelAndQuit })
    await act(async () => {})
    emitTray(busySurvey)

    expect(screen.getByRole('button', { name: '取消任务并退出' })).toBeTruthy()
    expect(screen.getByRole('button', { name: '继续跑，稍后再退' })).toBeTruthy()
    expect(screen.getByRole('button', { name: '收回托盘' })).toBeTruthy()

    fireEvent.click(screen.getByRole('button', { name: '取消任务并退出' }))
    expect(cancelAndQuit).toHaveBeenCalledTimes(1)
    expect(screen.getByRole('status').textContent).toContain('正在取消任务')
    await act(async () => { resolveCancel?.() })
  })

  it('收回托盘隐藏窗口并关闭对话框', async () => {
    const { hideToTray } = renderDialog()
    await act(async () => {})
    emitTray(busySurvey)
    fireEvent.click(screen.getByRole('button', { name: '收回托盘' }))
    expect(hideToTray).toHaveBeenCalledTimes(1)
    expect(screen.queryByRole('dialog')).toBeNull()
  })
})

describe('requestQuit', () => {
  beforeEach(() => {
    invoke.mockClear()
  })

  it('没有在跑的事时直接 quit_app，不弹窗', async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === 'quit_survey') return idleSurvey
      return undefined
    })
    await requestQuit()
    expect(invoke.mock.calls.map(call => call[0])).toEqual(['quit_survey', 'quit_app'])
  })

  it('有任务时把盘点结果交给对话框，不直接退出', async () => {
    invoke.mockImplementation(async (command: string) => {
      if (command === 'quit_survey') return busySurvey
      return undefined
    })
    const received: QuitSurvey[] = []
    const onLocal = (event: Event): void => {
      received.push((event as CustomEvent<QuitSurvey>).detail)
    }
    window.addEventListener(QUIT_DIALOG_EVENT, onLocal)
    await requestQuit()
    window.removeEventListener(QUIT_DIALOG_EVENT, onLocal)
    expect(received).toHaveLength(1)
    expect(received[0]?.activeSession).toBe('sess-1')
    expect(invoke.mock.calls.map(call => call[0])).toEqual(['quit_survey'])
  })
})
