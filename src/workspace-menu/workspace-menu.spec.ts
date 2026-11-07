import { describe, expect, it, vi } from 'vitest'

const opened = vi.fn(async (_path: string) => true)
const flags = { tauri: false }
vi.mock('../header-actions/open-workspace', () => ({
  openInSystemExplorer: (path: string) => opened(path),
  inTauri: () => flags.tauri,
}))

import { apply } from './index'

interface MenuAction {
  id: string
  label: string
  disabled?: boolean
  icon?: { key: string }
  failureLabel?: string
  run?: () => void | Promise<void>
}

interface Contribution {
  id: string
  target: string
  order: number
  resolve: (target: { path: string }, signal: AbortSignal) => readonly MenuAction[]
}

/** 记录注册结果的最小 ctx：词典取回键名，图标槽位与菜单贡献按注册顺序记录。 */
function harness() {
  const contributions: Contribution[] = []
  const icons: string[] = []
  const ctx = {
    effect: (fn: () => unknown) => { fn(); return () => {} },
    locale: { register: () => () => {}, bind: () => (key: string) => key },
    slots: {
      inject: (_name: string, callback: () => Generator) => {
        for (const entry of callback()) void entry
        return () => {}
      },
      register: (options: { key?: string }) => {
        if (options.key !== undefined) icons.push(options.key)
        return () => {}
      },
    },
    contextMenus: {
      register: (contribution: Contribution) => {
        contributions.push(contribution)
        return () => {}
      },
    },
  }
  apply(ctx as never)
  return { contributions, icons }
}

/** 取出 workspace 目标的贡献并解析一次。 */
function revealActions(targetPath = 'D:\\ws\\demo'): readonly MenuAction[] {
  const { contributions } = harness()
  const contribution = contributions.find(item => item.target === 'workspace')
  if (contribution === undefined) throw new Error('workspace contribution not registered')
  expect(contribution.id).toBe('shaniu.workspace.reveal')
  expect(contribution.order).toBe(200)
  return contribution.resolve({ path: targetPath }, new AbortController().signal)
}

describe('workspace menu', () => {
  it('注册 open-in-app.reveal 图标席位与 order 200 的菜单贡献（重命名与删除之间）', () => {
    const { icons } = harness()
    expect(icons).toContain('open-in-app.reveal')
  })

  it('浏览器 dev 无系统文件管理器：菜单项如实置灰，标签取资源管理器口径', () => {
    const [action] = revealActions()
    expect(action.id).toBe('reveal')
    expect(action.label).toBe('menu.reveal')
    expect(action.disabled).toBe(true)
    expect(action.icon?.key).toBe('open-in-app.reveal')
    expect(action.failureLabel).toBe('menu.revealFailed')
  })

  it('macOS 桌面取 Finder 口径', () => {
    document.documentElement.dataset.platform = 'darwin'
    try {
      const [action] = revealActions()
      expect(action.label).toBe('menu.finder')
    } finally {
      delete document.documentElement.dataset.platform
    }
  })

  it('启用时 run 在目标工作区目录上发起打开', async () => {
    flags.tauri = true
    try {
      const [action] = revealActions('D:\\ws\\demo')
      expect(action.disabled).toBe(false)
      await action.run?.()
      expect(opened).toHaveBeenCalledWith('D:\\ws\\demo')
    } finally {
      flags.tauri = false
    }
  })

  it('打开失败时 run 拒绝，由注册表给出 localized 失败提示', async () => {
    flags.tauri = true
    opened.mockResolvedValueOnce(false)
    try {
      const [action] = revealActions()
      await expect(action.run?.()).rejects.toThrow('menu.revealFailed')
    } finally {
      flags.tauri = false
      opened.mockResolvedValue(true)
    }
  })
})
