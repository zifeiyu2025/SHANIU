// @vitest-environment jsdom
/**
 * 沙箱可见面契约测试（2026-11-11 起：降级为**异常提示**）。
 *
 * 承诺四件事：
 *  1. 插件只占 `shell.bottom`，不再有会话头部网络开关；
 *  2. 强制完整（常态）时**整条不渲染**——档位归输入区的访问模式 chip；
 *  3. 逻辑沙箱（`enforcement = partial`）、读取失败、文件权限准备中 / 没准备好这四种
 *     异常必须可见；文件权限的两种**只给一行字**，不许有进度条、弹窗或拦人动作；
 *  4. 任何状态都不许出现第二套档位词（只读 / 工作区可写 / 完全放开），
 *     也不许把机制 / profile 这类实现细节重新搬回来。
 *
 * 第 4 条是反向对照：只断言"新文案在"不够——旧词留着不会让正向断言变红，
 * 却正是用户看到的「同一档位两套词」。
 */
import { describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { act, render, screen } from '@testing-library/react'
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
  ancestor: 'ready',
  note: 'AppContainer 路径/读隔离；受限令牌与 Job 作为纵深保留',
}

const LOGICAL_STATUS: SandboxStatusWire = {
  mode: 'workspace_write',
  os: 'logical',
  os_enabled: false,
  enforcement: 'partial',
  profile: '',
  ancestor: 'idle',
  note: '当前未启用 OS 沙箱：仅受限令牌 + Job（逻辑隔离），没有路径级写入边界',
}

/** 文件权限后台准备中：只该出现一行中性提示。 */
const PREPARING_STATUS: SandboxStatusWire = { ...OS_STATUS, ancestor: 'preparing' }

/** 文件权限没准备好：一行警示，但**不许**拦人（无弹窗、无禁用、无额外按钮）。 */
const NEEDS_ADMIN_STATUS: SandboxStatusWire = { ...OS_STATUS, ancestor: 'needs_admin' }

/** 档位词的两种写法都必须绝迹：自造词与 chip 词都不该由本组件输出。 */
const TIER_WORDS = ['只读', '工作区可写', '完全放开', '仅可查看', '工作区内修改', '完全权限'] as const

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

/** 让挂载后的异步状态查询落地。 */
async function flush(): Promise<void> {
  await act(async () => { await Promise.resolve() })
}

describe('src/sandbox-status：插件接缝', () => {
  it('只依赖 slots 与 locale，且只占 shell.bottom（会话头部开关已删）', () => {
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

describe('src/sandbox-status：异常才出现的提示条', () => {
  it('强制完整时整条不渲染（档位交还输入区 chip）', async () => {
    const { container } = render(barElement(fakeWire()))
    await flush()
    expect(container.innerHTML).toBe('')
    for (const word of TIER_WORDS) expect(document.body.textContent).not.toContain(word)
  })

  it('逻辑沙箱：出现一条警示提示，且不带档位 / 机制 / profile', async () => {
    render(barElement(fakeWire(LOGICAL_STATUS)))
    const notice = await screen.findByText(zh['notice.partial'])

    const bar = notice.closest('.shaniu-sandbox-notice')
    expect(bar).toBeTruthy()
    expect(bar?.getAttribute('data-tone')).toBe('warn')

    const text = document.body.textContent ?? ''
    for (const word of TIER_WORDS) expect(text).not.toContain(word)
    for (const detail of ['AppContainer', '逻辑沙箱', '机制', 'Shaniu.Sandbox.v1']) {
      expect(text).not.toContain(detail)
    }

    // 只剩刷新按钮（aria-label 是"刷新沙箱状态"），没有授权 / 切换入口。
    const buttons = screen.getAllByRole('button')
    expect(buttons).toHaveLength(1)
    expect(buttons[0]?.getAttribute('aria-label')).toBe(zh['refresh'])
  })

  it('状态读取失败要变成可见反馈，而不是留一条"看着正常"的空白', async () => {
    render(barElement(fakeWire(new Error('status unavailable'))))
    const bar = await screen.findByRole('status')
    expect(bar.getAttribute('data-tone')).toBe('error')
    expect(bar.textContent).toContain('status unavailable')
    for (const word of TIER_WORDS) expect(document.body.textContent).not.toContain(word)
  })

  /** §4.2 反向对照：网络维度删掉之后，界面不许残留任何网络口径。 */
  it('三种状态下都不出现网络字样', async () => {
    for (const status of [OS_STATUS, LOGICAL_STATUS, new Error('boom')]) {
      const { unmount } = render(barElement(fakeWire(status)))
      await flush()
      const text = document.body.textContent ?? ''
      for (const stale of ['网络', '已断', '已放行', '默认拒绝', '允许联网', '撤销联网', '未生效（无 OS 隔离）']) {
        expect(text).not.toContain(stale)
      }
      unmount()
    }
  })

  it('文件权限准备中：一行中性提示，无按钮，落到终态自动消失', async () => {
    vi.useFakeTimers()
    try {
      let calls = 0
      const wire: SandboxWire = {
        status: async () => (calls++ === 0 ? PREPARING_STATUS : OS_STATUS),
      }
      render(barElement(wire))
      await act(async () => { await Promise.resolve() })

      const notice = screen.getByText(zh['notice.preparing'])
      expect(notice.closest('.shaniu-sandbox-notice')?.getAttribute('data-tone')).toBe('info')
      // 只要一行字：不给按钮、不给进度条之类的可交互物。
      expect(screen.queryAllByRole('button')).toHaveLength(0)
      expect(screen.queryByRole('progressbar')).toBeNull()

      // 每秒自查一次；状态到 ready 后提示消失并停止轮询。
      await act(async () => { await vi.advanceTimersByTimeAsync(1000) })
      expect(screen.queryByText(zh['notice.preparing'])).toBeNull()
      const afterStop = calls
      await act(async () => { await vi.advanceTimersByTimeAsync(3000) })
      expect(calls).toBe(afterStop)
    } finally {
      vi.useRealTimers()
    }
  })

  it('文件权限没准备好：只警示，不弹窗、不禁用、不给额外入口', async () => {
    render(barElement(fakeWire(NEEDS_ADMIN_STATUS)))
    const notice = await screen.findByText(zh['notice.needs_admin'])

    expect(notice.closest('.shaniu-sandbox-notice')?.getAttribute('data-tone')).toBe('warn')
    // 拦人动作一个都不许有：没有「立即提权」按钮、没有 modal、没有禁用态。
    expect(screen.queryAllByRole('button')).toHaveLength(0)
    expect(screen.queryByRole('dialog')).toBeNull()
    expect(document.querySelector('[disabled]')).toBeNull()
    // 文案不许出现实现细节（那些词用户看不懂，也会泄漏内部结构）。
    const text = document.body.textContent ?? ''
    for (const term of ['目录层级', 'DACL', 'ACE', 'SID', '权限项', 'ACL']) {
      expect(text).not.toContain(term)
    }
  })
})
