/**
 * 桌面环境声明——Tauri 壳的 preload 等价物。
 *
 * DSH 客户端用 `html[data-platform]` 判定运行时：有值即 desktop。桌面运行时要求
 * `dshDesktop.keyboard` 键盘桥（缺失会让 ShortcutsService 构造即抛错），且配置型
 * 快捷键改由桥转发（DOM 只再送固定动作）。官方 Electron preload 提供这两件，
 * SHANIU 的 Tauri 壳在这里补齐：键盘桥把主文档 keydown 按当前修订号转发，
 * 键位偏好用 localStorage 落地（真实落盘属 SH-004/026 的接线对象）。
 *
 * 只在 Tauri 内安装：纯浏览器保留 web 运行时——浏览器会抢 Ctrl+O / Ctrl+N，
 * web 默认键位本就该带 Alt。
 */
import { initialShortcutConfig, ShortcutPersistence } from '@deepseek-ai/dsh-client-shortcuts/protocol'
import type {
  DesktopKeyboardApi, DesktopShortcutsApi, ShortcutConfigSnapshot, ShortcutDefinition,
  ShortcutEdit, ShortcutPlatform, ShortcutRevision, ShortcutSaveResult,
} from '@deepseek-ai/dsh-client-shortcuts/protocol'

declare global {
  interface Window {
    /** 桌面壳暴露给客户端的受限能力面（官方 Electron preload 的等价物）。 */
    dshDesktop?: {
      readonly protocolVersion: number
      readonly shortcuts: DesktopShortcutsApi
      readonly keyboard: DesktopKeyboardApi
    }
  }
}

/** 与 web 适配层同键：桌面窗口与浏览器联调共享同一份键位偏好。 */
const STORAGE_KEY = 'dsh.keybindings.v1'

/** `html[data-platform]` 的规范值取自 process.platform 词表（detectEnvironment 按 /win|darwin|mac/ 匹配）。 */
function detectDevice(): { platform: ShortcutPlatform; marker: string } {
  const device = typeof navigator === 'undefined' ? '' : navigator.platform
  if (/win/iu.test(device)) return { platform: 'windows', marker: 'win32' }
  if (/darwin|mac|iphone|ipad/iu.test(device)) return { platform: 'macos', marker: 'darwin' }
  return { platform: 'linux', marker: 'linux' }
}

/** 键位偏好的桌面适配层：单写者事务与修订号都由 ShortcutPersistence 统一供给。 */
class DesktopShortcuts implements DesktopShortcutsApi {
  private readonly persistence: ShortcutPersistence
  private readonly listeners = new Set<(snapshot: ShortcutConfigSnapshot) => void>()
  private snapshot: ShortcutConfigSnapshot = initialShortcutConfig()

  constructor(private readonly window: Window, platform: ShortcutPlatform) {
    this.persistence = new ShortcutPersistence({
      read: () => this.window.localStorage.getItem(STORAGE_KEY),
      write: (raw) => { this.window.localStorage.setItem(STORAGE_KEY, raw) },
    }, 'desktop', platform, false, (snapshot) => {
      this.snapshot = snapshot
      for (const listener of [...this.listeners]) listener(snapshot)
    })
  }

  /** 当前接受的修订号：键盘桥随每次输入带给注册表，修订不符的输入会被丢弃。 */
  revision(): ShortcutRevision {
    return this.snapshot.revision
  }

  get(definitions: readonly ShortcutDefinition[]): Promise<ShortcutConfigSnapshot> {
    this.persistence.setDefinitions(definitions)
    return this.persistence.readCurrent()
  }

  edit(edit: ShortcutEdit, revision: ShortcutRevision): Promise<ShortcutSaveResult> {
    return this.persistence.edit(edit, revision)
  }

  subscribe(listener: (snapshot: ShortcutConfigSnapshot) => void): () => void {
    this.listeners.add(listener)
    return () => { this.listeners.delete(listener) }
  }

  recording(_active: boolean): Promise<void> {
    return Promise.resolve()
  }
}

/** 键盘桥：把主文档 keydown 按官方输入面（DesktopShortcutInput）转发给注册表。 */
function createKeyboardBridge(shortcuts: DesktopShortcuts, window: Window): DesktopKeyboardApi {
  return {
    subscribe(listener) {
      const onKeyDown = (event: KeyboardEvent): void => {
        // 输入法组合中的按键不是命令手势（与官方 preload 的过滤面一致）
        if (event.isComposing) return
        listener({
          kind: 'keyboard',
          revision: shortcuts.revision(),
          frameName: '',
          code: event.code,
          control: event.ctrlKey,
          alt: event.altKey,
          shift: event.shiftKey,
          meta: event.metaKey,
          repeat: event.repeat,
        })
      }
      // 捕获阶段先于页面内控件：与原生输入的「由窗口路由」语义对齐
      window.addEventListener('keydown', onKeyDown, true)
      return () => { window.removeEventListener('keydown', onKeyDown, true) }
    },
    // 关窗由标题栏按钮与托盘负责，桌面快捷键不代理
    closeWindow: () => Promise.resolve(),
  }
}

/**
 * 在 Tauri 壳里声明桌面运行时：写 `html[data-platform]` 并挂 `dshDesktop` 能力面。
 * 必须在 bootShaniu（构造 ShortcutsService）之前调用，否则快捷键会按 web 键位初始化。
 * @param window - 产品主窗口。
 */
export function installDesktopEnvironment(window: Window = globalThis.window): void {
  if (!('__TAURI_INTERNALS__' in window)) return
  const { platform, marker } = detectDevice()
  document.documentElement.dataset.platform = marker
  const shortcuts = new DesktopShortcuts(window, platform)
  window.dshDesktop = {
    protocolVersion: 1,
    shortcuts,
    keyboard: createKeyboardBridge(shortcuts, window),
  }
}
