// @vitest-environment jsdom
/**
 * 「默认目录」说明行契约测试。
 *
 * 这一行存在的理由是一条行为变更：`data\Workspace` 不再无条件创建，只在
 * "这次真的以默认根工作"时创建。所以文案必须能回答三件事：
 *  1. 默认根是哪个路径（未配置 → `<数据目录>\Workspace`）；
 *  2. 什么时候才会创建它；
 *  3. 当前生效的工作区是不是它（配置了别的根时要明说）。
 *
 * 反向对照：把文案改回「当前未使用」，第 1、2 条用例必红（默认根本身仍在，
 * 只是没被使用时会创建——写成"未使用"就是与代码不符的断言）。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { render, screen, waitFor } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { zh } from './locales'

const DEFAULT_ROOT = 'C:\\Users\\me\\AppData\\Roaming\\SHANIU\\Workspace'

const state = vi.hoisted(() => ({
  dirs: { workspaceRoot: 'C:\\Users\\me\\AppData\\Roaming\\SHANIU\\Workspace' },
  settings: { workspaceRoot: 'C:\\Users\\me\\AppData\\Roaming\\SHANIU\\Workspace' },
  fail: false,
}))

vi.mock('../transport/tauri', () => ({
  tauriBridge: () => ({
    invoke: async (command: string) => {
      if (state.fail) throw new Error('ipc unavailable')
      return command === 'get_product_dirs' ? state.dirs : state.settings
    },
  }),
}))

import { DefaultDirRow, samePath } from './DefaultDirRow'
import { apply, inject } from './index'

/** 词典直译（测试只关心键是否被用到、参数是否落位）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

function renderRow(): HTMLElement {
  const Row = DefaultDirRow as unknown as (props: Record<string, unknown>) => ReactNode
  return render(<Row t={translate} />).container
}

beforeEach(() => {
  state.dirs = { workspaceRoot: DEFAULT_ROOT }
  state.settings = { workspaceRoot: DEFAULT_ROOT }
  state.fail = false
})

afterEach(() => { vi.restoreAllMocks() })

describe('src/default-dir：文案与代码行为一致', () => {
  it('正在使用默认目录：显示路径与创建条件，不重复说当前工作区', async () => {
    renderRow()
    await screen.findByText(zh.title)
    expect(screen.getByText(DEFAULT_ROOT)).toBeTruthy()
    expect(screen.getByText(zh.hint)).toBeTruthy()
    // 生效根就是默认根：再补一句「当前工作区是 …」只是噪音。
    expect(screen.queryByText(/当前工作区是/)).toBeNull()
    // 默认根仍然是默认根，不能写成"当前未使用"。
    expect(screen.queryByText(/当前未使用/)).toBeNull()
  })

  it('配置了别的根：默认目录照常显示，并明说当前工作区不是它', async () => {
    state.settings = { workspaceRoot: 'D:/MyWorkspace/SHANIU' }
    renderRow()
    await screen.findByText(zh.title)
    expect(screen.getByText(DEFAULT_ROOT)).toBeTruthy()
    expect(screen.getByText('当前工作区是 D:/MyWorkspace/SHANIU（不是默认目录）。')).toBeTruthy()
    expect(screen.queryByText(/当前未使用/)).toBeNull()
  })

  it('读取失败时整行不显示，不猜一个路径', async () => {
    state.fail = true
    const container = renderRow()
    await waitFor(() => expect(container.firstChild).toBeNull())
    expect(screen.queryByText(zh.title)).toBeNull()
  })

  it('路径比较忽略大小写、分隔符与尾部分隔符', () => {
    expect(samePath('D:\\MyWorkspace\\SHANIU', 'd:/MyWorkspace/SHANIU/')).toBe(true)
    expect(samePath('D:\\MyWorkspace\\SHANIU', 'D:\\MyWorkspace\\SHANIU2')).toBe(false)
  })
})

describe('src/default-dir：插件接缝', () => {
  it('占用 settings.general.item，键控 default-dir', () => {
    expect(inject).toEqual(['slots', 'locale'])

    const waited: string[] = []
    const registered: { options: Record<string, unknown>; component: unknown }[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('slots', {
      inject: (key: string, contribution: () => void) => {
        waited.push(key)
        contribution()
        return () => {}
      },
      register: (options: Record<string, unknown>, component: unknown) => {
        registered.push({ options, component })
        return () => {}
      },
    } as never)

    apply(ctx)
    expect(waited).toEqual(['settings.general.item'])
    expect(registered).toHaveLength(1)
    expect(registered[0]?.options).toMatchObject({ id: 'default-dir', locale: 'shaniuDefaultDir' })
    expect(registered[0]?.component).toBe(DefaultDirRow)
  })
})
