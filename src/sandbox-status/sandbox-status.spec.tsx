// @vitest-environment jsdom
/**
 * 沙箱可见面契约测试。
 *
 * 覆盖本卡（SH-021 UI 可见性）承诺的三件事：
 *  1. 插件只占 `shell.bottom`（常驻），**不再有**会话头部网络开关；
 *  2. OS 沙箱态如实显示模式 / 强制完整度 / 机制 / profile，且**不**出现「未启用 OS 沙箱」；
 *  3. 逻辑沙箱态必须出现显眼警告与不同的数据属性（视觉分档由 CSS 承担）。
 *
 * 网络不再是策略维度（2026-11-10），所以这里还多一条反向对照：**状态条不许再出现
 * 任何网络字样**，数据面也只允许调 `sandbox_status`。只断言"新东西在"是不够的——
 * 旧文案留着不会让任何正向断言变红，却会让用户以为还有一道可开的闸。
 */
import { describe, expect, it } from 'vitest'
import type { ReactNode } from 'react'
import { render, screen } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as sandboxApply, inject as sandboxInject } from './index'
import { SandboxStatusBar } from './SandboxStatusBar'
import { zh } from './locales'
import { hostSandboxWire, type SandboxStatusWire, type SandboxWire } from './wire'

/** 词典直译（测试只关心键是否被用到，不关心八语种）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

const OS_STATUS: SandboxStatusWire = {
  mode: 'workspace_write',
  os: 'app_container',
  os_enabled: true,
  enforcement: 'full',
  profile: 'Shaniu.Sandbox.v1',
  note: 'AppContainer 路径/读隔离；受限令牌与 Job 作为纵深保留',
}

const LOGICAL_STATUS: SandboxStatusWire = {
  mode: 'workspace_write',
  os: 'logical',
  os_enabled: false,
  enforcement: 'partial',
  profile: '',
  note: '当前未启用 OS 沙箱：仅受限令牌 + Job（逻辑隔离），没有路径级写入边界',
}

/** 脚本化的数据面：只实现状态查询。 */
function fakeWire(status: SandboxStatusWire | Error = OS_STATUS): SandboxWire {
  return {
    status: async () => {
      if (status instanceof Error) throw status
      return status
    },
  }
}

/** 槽位标准套件里的其余 hook 与本用例无关：按渲染契约只喂它真正用到的几件。 */
function barElement(wire: SandboxWire): ReactNode {
  const Bar = SandboxStatusBar as unknown as (props: Record<string, unknown>) => ReactNode
  return <Bar t={translate} wire={wire} />
}

describe('src/sandbox-status：插件接缝', () => {
  it('只依赖 slots 与 locale，且只占常驻状态条（会话头部开关已删）', () => {
    expect(sandboxInject).toEqual(['slots', 'locale'])

    const waited: string[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('slots', {
      inject: (key: string) => { waited.push(key); return () => {} },
      register: () => () => {},
    } as never)

    sandboxApply(ctx)
    expect(waited).toEqual(['shell.bottom'])
    // 反向对照：网络开关曾经挂在会话头部，必须确认它没有偷偷回来。
    expect(waited).not.toContain('conversation.session.header.utilities')
  })

  it('数据面只允许调 sandbox_status，网络三命令不许残留', async () => {
    const invoked: string[] = []
    const bridge = {
      invoke: async (command: string) => {
        invoked.push(command)
        return OS_STATUS
      },
    }
    const wire = hostSandboxWire(bridge as never)
    await wire.status()
    expect(invoked).toEqual(['sandbox_status'])
    // 类型面也锁住：SandboxWire 上没有 grants/grant/revoke 可用。
    expect(Object.keys(wire)).toEqual(['status'])
  })
})

describe('src/sandbox-status：常驻状态呈现', () => {
  it('OS 沙箱态显示 AppContainer 与 profile，且没有任何"未启用"警告', async () => {
    render(barElement(fakeWire()))
    await screen.findByText('OS 沙箱（AppContainer）')
    expect(screen.getByText('Shaniu.Sandbox.v1')).toBeTruthy()
    expect(screen.getByText('工作区可写')).toBeTruthy()
    expect(screen.getByText(zh['enforcement.full'])).toBeTruthy()
    expect(document.body.textContent).not.toContain('当前未启用 OS 沙箱')
    expect(document.querySelector('.shaniu-sandbox-bar')?.getAttribute('data-os')).toBe('os')
  })

  it('逻辑沙箱态必须显眼：警告文字 + 不同的 data-os', async () => {
    render(barElement(fakeWire(LOGICAL_STATUS)))
    await screen.findByText('逻辑沙箱')
    const warning = screen.getByText(zh['os.logicalWarning'])
    expect(warning.closest('.shaniu-sandbox-bar-warning')).toBeTruthy()
    expect(document.querySelector('.shaniu-sandbox-bar')?.getAttribute('data-os')).toBe('logical')
    expect(document.body.textContent).not.toContain('Shaniu.Sandbox.v1')
    // 逻辑沙箱没有路径级边界：强制完整度必须自报 partial，不许随机制一起"看着还行"。
    expect(screen.getByText(zh['enforcement.partial'])).toBeTruthy()
  })

  /** §4.2 反向对照：网络维度删掉之后，界面不许残留任何网络口径。 */
  it('状态条不出现任何网络字样，也没有联网/撤销按钮', async () => {
    for (const status of [OS_STATUS, LOGICAL_STATUS]) {
      const { unmount } = render(barElement(fakeWire(status)))
      await screen.findByText(zh['label.sandbox'])
      const text = document.body.textContent ?? ''
      for (const stale of ['网络', '已断', '已放行', '默认拒绝', '允许联网', '撤销联网', '未生效（无 OS 隔离）']) {
        expect(text).not.toContain(stale)
      }
      // 只剩刷新按钮（aria-label 是"刷新沙箱状态"），没有授权入口。
      const buttons = screen.getAllByRole('button')
      expect(buttons).toHaveLength(1)
      expect(buttons[0]?.getAttribute('aria-label')).toBe(zh['refresh'])
      unmount()
    }
  })

  it('状态读取失败要变成可见反馈，而不是留一条"看着正常"的空白条', async () => {
    render(barElement(fakeWire(new Error('status unavailable'))))
    await screen.findByText(/status unavailable/, { selector: '[data-tone="error"]' })
  })
})
