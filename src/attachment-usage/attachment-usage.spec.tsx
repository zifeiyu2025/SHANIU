// @vitest-environment jsdom
/**
 * 「附件占用」行契约测试。
 *
 * 这一行只做一件事：把 `attachments/` 的量级摆出来。三条必须成立：
 *  1. 成功时显示「文件数 / 字节数」；
 *  2. 读取失败显示「—」，**不能显示 0**（0 会被读成「没有附件」）；
 *  3. 只读，没有清理入口。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { render, screen, waitFor } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { zh } from './locales'

const state = vi.hoisted(() => ({
  stats: { fileCount: 12, totalBytes: 1536 },
  fail: false,
}))

vi.mock('../transport/tauri', () => ({
  tauriBridge: () => ({
    invoke: async () => {
      if (state.fail) throw new Error('ipc unavailable')
      return state.stats
    },
  }),
}))

import { AttachmentUsageRow, formatBytes } from './AttachmentUsageRow'
import { apply, inject } from './index'

/** 词典直译（测试只关心键是否被用到、参数是否落位）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

function renderRow(): HTMLElement {
  const Row = AttachmentUsageRow as unknown as (props: Record<string, unknown>) => ReactNode
  return render(<Row t={translate} />).container
}

beforeEach(() => {
  state.stats = { fileCount: 12, totalBytes: 1536 }
  state.fail = false
})

afterEach(() => { vi.restoreAllMocks() })

describe('src/attachment-usage：只读展示', () => {
  it('成功时显示文件数与人类可读的字节数', async () => {
    renderRow()
    expect(await screen.findByText('12 个文件 / 1.5 KB')).toBeTruthy()
    expect(screen.getByText(zh.hint)).toBeTruthy()
  })

  it('读取失败显示「—」，不显示 0', async () => {
    state.fail = true
    const container = renderRow()
    await screen.findByText(zh.title)
    await waitFor(() => expect(container.textContent).toContain('—'))
    expect(container.textContent).not.toContain('0 个文件')
  })

  it('空目录是合法的 0，不是「—」', async () => {
    state.stats = { fileCount: 0, totalBytes: 0 }
    renderRow()
    expect(await screen.findByText('0 个文件 / 0 B')).toBeTruthy()
  })
})

describe('src/attachment-usage：字节格式化', () => {
  it('B 不带小数，KB/MB 保留一位', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(1023)).toBe('1023 B')
    expect(formatBytes(1024)).toBe('1.0 KB')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(5 * 1024 * 1024)).toBe('5.0 MB')
  })
})

describe('src/attachment-usage：插件接缝', () => {
  it('占用 settings.general.item，键控 attachment-usage', () => {
    expect(inject).toEqual(['slots', 'locale'])
    const waited: string[] = []
    const registered: { options: Record<string, unknown>; component: unknown }[] = []
    const ctx = {
      effect: (run: () => () => void) => { run() },
      provide: () => {},
      locale: { register: () => () => {}, bind: () => translate },
      slots: {
        inject: (name: string, run: () => unknown) => { waited.push(name); run() },
        register: (options: Record<string, unknown>, component: unknown) => {
          registered.push({ options, component })
          return () => {}
        },
      },
    } as unknown as Context

    apply(ctx)
    expect(waited).toEqual(['settings.general.item'])
    expect(registered).toHaveLength(1)
    expect(registered[0]?.options).toMatchObject({ id: 'attachment-usage', locale: 'shaniuAttachmentUsage' })
    expect(registered[0]?.component).toBe(AttachmentUsageRow)
  })
})
