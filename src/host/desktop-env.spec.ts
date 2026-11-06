import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ShortcutConfigSnapshot, ShortcutDefinition } from '@deepseek-ai/dsh-client-shortcuts/protocol'
import { installDesktopEnvironment } from './desktop-env'

/** 模拟 Tauri webview：官方 Electron preload 的等价物由壳自己安装。 */
function installAsDesktop(): void {
  ;(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {}
  vi.spyOn(window.navigator, 'platform', 'get').mockReturnValue('Win32')
  installDesktopEnvironment(window)
}

const definition = {
  id: 'workspace.add',
  defaults: { 'desktop:windows': { code: 'KeyO', modifiers: ['primary'] } },
} as unknown as ShortcutDefinition

afterEach(() => {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__
  delete window.dshDesktop
  delete document.documentElement.dataset.platform
  window.localStorage.clear()
  vi.restoreAllMocks()
})

describe('桌面环境声明', () => {
  it('纯浏览器不声明桌面运行时，也不写 platform 标记', () => {
    installDesktopEnvironment(window)
    expect(window.dshDesktop).toBeUndefined()
    expect(document.documentElement.dataset.platform).toBeUndefined()
  })

  it('Tauri 内写规范 platform 标记并挂出 dshDesktop 能力面', () => {
    installAsDesktop()
    expect(document.documentElement.dataset.platform).toBe('win32')
    expect(window.dshDesktop?.protocolVersion).toBe(1)
    expect(typeof window.dshDesktop?.keyboard.subscribe).toBe('function')
    expect(typeof window.dshDesktop?.shortcuts.get).toBe('function')
  })

  it('键盘桥把 keydown 按当前修订号转发，退订后不再收', async () => {
    installAsDesktop()
    const shortcuts = window.dshDesktop!.shortcuts
    const snapshot = await shortcuts.get([definition])

    const received: unknown[] = []
    const off = window.dshDesktop!.keyboard.subscribe(input => { received.push(input) })
    window.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyO', ctrlKey: true }))
    expect(received).toEqual([{
      kind: 'keyboard',
      revision: snapshot.revision,
      frameName: '',
      code: 'KeyO',
      control: true,
      alt: false,
      shift: false,
      meta: false,
      repeat: false,
    }])

    off()
    window.dispatchEvent(new KeyboardEvent('keydown', { code: 'KeyN', ctrlKey: true }))
    expect(received).toHaveLength(1)
  })

  it('键位偏好走单写者：get 给出 ready 快照，edit 换新修订号并推给订阅者', async () => {
    installAsDesktop()
    const shortcuts = window.dshDesktop!.shortcuts
    const before = await shortcuts.get([definition])
    expect(before.status).toBe('ready')
    expect(before.usingDefaults).toBe(true)

    const published: ShortcutConfigSnapshot[] = []
    const off = shortcuts.subscribe(snapshot => { published.push(snapshot) })
    const saved = await shortcuts.edit(
      { type: 'set', id: definition.id, binding: { code: 'KeyP', modifiers: ['primary'] } },
      before.revision,
    )
    expect(saved.status).toBe('saved')
    expect(saved.snapshot.revision).not.toBe(before.revision)
    expect(saved.snapshot.usingDefaults).toBe(false)
    expect(published.some(snapshot => snapshot.revision === saved.snapshot.revision)).toBe(true)
    off()
  })
})
