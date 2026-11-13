// @vitest-environment jsdom
/**
 * 全局待审批横幅契约测试。
 *
 * 承诺四件事：
 *  1. 没有待审批就整条不渲染（常态零占位）；
 *  2. 每行带自己的会话名与工具名——不是只显示一个数字；
 *  3. 点击只切到**该行**的会话，再聚焦审批卡；多会话共用窗体时也不看"当前会话"；
 *  4. 心跳中断时把后果说清楚（TTL 到点按拒绝收口），而不是只说"再等等"。
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { act, cleanup, fireEvent, render, screen } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as bannerApply, inject as bannerInject } from './index'
import { ApprovalBanner, type ApprovalBannerProps } from './ApprovalBanner'
import { FOCUS_CLASS, focusApprovalCard } from './focus'
import { hostApprovalBannerWire, type ApprovalBannerWire } from './wire'
import { zh } from './locales'
import type {
  PendingApprovalWire, PendingApprovalsSnapshotWire,
} from '../transport/pending-approval'

/** 词典直译（键是否被用到才是本用例关心的）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

/** 一条待审批（默认属于会话 `sess-a`）。 */
function approval(overrides: Partial<PendingApprovalWire> = {}): PendingApprovalWire {
  return {
    requestId: 'appr-1',
    sessionId: 'sess-a',
    agentId: 'root',
    tool: 'unified_exec',
    risk: 'execute',
    args: { command: 'git status' },
    reason: '需要执行命令',
    callId: 'call-1',
    taskSummary: null,
    batchScope: null,
    createdAtMs: 1,
    ...overrides,
  }
}

/** 快照（默认一条待审批、心跳正常）。 */
function snapshot(
  approvals: PendingApprovalWire[],
  overrides: Partial<PendingApprovalsSnapshotWire> = {},
): PendingApprovalsSnapshotWire {
  return { approvals, revision: 1, heartbeatMs: 5000, ttlMs: 15000, attended: true, ...overrides }
}

/** 脚本化数据面：查询返回固定快照，订阅可手动推送。 */
function fakeWire(initial: PendingApprovalsSnapshotWire): {
  wire: ApprovalBannerWire
  push: (next: PendingApprovalsSnapshotWire) => void
} {
  let handler: ((next: PendingApprovalsSnapshotWire) => void) | undefined
  return {
    wire: {
      pending: async () => initial,
      subscribe: (next) => {
        handler = next
        return () => { handler = undefined }
      },
    },
    push: (next) => { handler?.(next) },
  }
}

/** 槽位标准套件里与本用例无关的 hook 不喂，只喂组件真正用到的几件。 */
function bannerElement(props: Partial<ApprovalBannerProps> & { wire: ApprovalBannerWire }): ReactNode {
  const Banner = ApprovalBanner as unknown as (input: Record<string, unknown>) => ReactNode
  return <Banner t={translate} titleOf={(id: string) => `会话-${id}`} {...props} />
}

/** 让挂载后的异步快照查询落地。 */
async function flush(): Promise<void> {
  await act(async () => { await Promise.resolve() })
}

afterEach(() => {
  cleanup()
  document.body.innerHTML = ''
  vi.useRealTimers()
})

describe('src/approval-banner：插件接缝', () => {
  it('依赖会话列表与跨会话导航，且只占 shell.overlay', () => {
    expect(bannerInject).toEqual(['slots', 'locale', 'sessions', 'uiWorkspace'])

    const waited: string[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('slots', {
      inject: (key: string) => { waited.push(key); return () => {} },
      register: () => () => {},
    } as never)
    ctx.provide('sessions', { list: { getSnapshot: () => ({ byId: {} }) } } as never)
    ctx.provide('uiWorkspace', { openSession: () => {} } as never)

    bannerApply(ctx)
    // 只占 overlay：`shell.bottom` 是沙箱提示的单一占用位，后注册会顶掉前者。
    expect(waited).toEqual(['shell.overlay'])
  })

  it('数据面只查待审批，不自行回执（回执只归审批卡）', async () => {
    const invoked: string[] = []
    let subscribed = false
    const bridge = {
      invoke: async (command: string) => {
        invoked.push(command)
        return snapshot([])
      },
      listenApprovalPending: () => {
        subscribed = true
        return () => {}
      },
    }
    const wire = hostApprovalBannerWire(bridge as never)
    await wire.pending()
    wire.subscribe(() => {})
    expect(invoked).toEqual(['pending_approvals'])
    expect(subscribed).toBe(true)
    // 反向对照：横幅没有 approve 面，不能绕过审批卡自己放行。
    expect(Object.keys(wire).sort()).toEqual(['pending', 'subscribe'])
  })
})

describe('src/approval-banner：渲染', () => {
  it('没有待审批时整条不渲染', async () => {
    const { wire } = fakeWire(snapshot([]))
    const { container } = render(bannerElement({ wire }))
    await flush()
    expect(container.innerHTML).toBe('')
  })

  it('每条请求一行：带会话名与工具名，不是只有一个数字', async () => {
    const { wire } = fakeWire(snapshot([
      approval({ requestId: 'appr-1', sessionId: 'sess-a', tool: 'unified_exec' }),
      approval({ requestId: 'appr-2', sessionId: 'sess-b', tool: 'apply_patch', risk: 'write_in_workspace' }),
    ]))
    render(bannerElement({ wire }))
    await flush()

    expect(screen.getByText(zh['banner.title'].replace('{count}', '2'))).toBeTruthy()
    expect(screen.getByText('会话-sess-a')).toBeTruthy()
    expect(screen.getByText('会话-sess-b')).toBeTruthy()
    expect(screen.getByText(zh['banner.tool'].replace('{tool}', 'unified_exec'))).toBeTruthy()
    expect(screen.getByText(zh['banner.tool'].replace('{tool}', 'apply_patch'))).toBeTruthy()
    expect(screen.getAllByRole('button')).toHaveLength(2)
  })

  it('超过三行时折叠，只提示还有多少条', async () => {
    const { wire } = fakeWire(snapshot([
      approval({ requestId: 'a1' }),
      approval({ requestId: 'a2', sessionId: 'sess-b' }),
      approval({ requestId: 'a3', sessionId: 'sess-c' }),
      approval({ requestId: 'a4', sessionId: 'sess-d' }),
      approval({ requestId: 'a5', sessionId: 'sess-e' }),
    ]))
    render(bannerElement({ wire }))
    await flush()
    expect(screen.getAllByRole('button')).toHaveLength(3)
    expect(screen.getByText(zh['banner.more'].replace('{count}', '2'))).toBeTruthy()
  })

  it('心跳中断：说清后果（TTL 秒数 + 按拒绝收口），不只说再等等', async () => {
    const { wire } = fakeWire(snapshot([approval()], { attended: false, ttlMs: 15000 }))
    render(bannerElement({ wire }))
    const banner = await screen.findByRole('status')
    expect(banner.getAttribute('data-attended')).toBe('false')
    const text = banner.textContent ?? ''
    expect(text).toContain('15 秒')
    expect(text).toContain('按拒绝收口')
    expect(text).not.toContain(zh['banner.hint'])
  })

  it('请求裁决后随快照消失，横幅不会留着一条已完成的待办', async () => {
    const fake = fakeWire(snapshot([approval({ requestId: 'appr-1' })]))
    render(bannerElement({ wire: fake.wire }))
    await flush()
    expect(screen.getAllByRole('button')).toHaveLength(1)

    await act(async () => {
      fake.push(snapshot([], { revision: 2 }))
    })
    expect(screen.queryAllByRole('button')).toHaveLength(0)
    expect(screen.queryByRole('status')).toBeNull()
  })
})

describe('src/approval-banner：定位', () => {
  it('点哪一行就切到哪一行自己的会话，再聚焦卡片（不读当前会话）', async () => {
    const { wire } = fakeWire(snapshot([
      approval({ requestId: 'appr-a', sessionId: 'sess-a' }),
      approval({ requestId: 'appr-b', sessionId: 'sess-b' }),
    ]))
    const opened: string[] = []
    const focused: string[] = []
    render(bannerElement({
      wire,
      openSession: (sessionId: string) => { opened.push(sessionId) },
      focusApproval: () => { focused.push('focus') },
    }))
    await flush()

    const rows = screen.getAllByRole('button')
    // 第二行属于 sess-b：即使列表顺序、当前会话都不指向它，也必须开 sess-b。
    fireEvent.click(rows[1] as HTMLElement)
    expect(opened).toEqual(['sess-b'])
    // 先切会话、再聚焦：顺序固定，否则会去聚焦上一个会话的卡。
    expect(focused).toEqual(['focus'])
  })

  it('聚焦只认 DOM 锚点：旧会话的卡被替换后才聚焦新卡', async () => {
    vi.useFakeTimers()
    const stale = document.createElement('div')
    stale.setAttribute('data-approval-key', 'approval:1')
    document.body.append(stale)

    focusApprovalCard(document, stale)
    await vi.advanceTimersByTimeAsync(0)
    // 切换还没完成：旧卡还在，不能把焦点丢给它。
    expect(stale.classList.contains(FOCUS_CLASS)).toBe(false)

    stale.remove()
    const fresh = document.createElement('div')
    fresh.setAttribute('data-approval-key', 'approval:2')
    document.body.append(fresh)
    await vi.advanceTimersByTimeAsync(50)
    expect(fresh.classList.contains(FOCUS_CLASS)).toBe(true)

    // 高亮只是短暂提示，不改变卡片状态。
    await vi.advanceTimersByTimeAsync(2400)
    expect(fresh.classList.contains(FOCUS_CLASS)).toBe(false)
  })

  it('目标就是当前会话（同一张卡）：短宽限后直接聚焦，不会一直等到超时', async () => {
    vi.useFakeTimers()
    const card = document.createElement('div')
    card.setAttribute('data-approval-key', 'approval:1')
    document.body.append(card)

    focusApprovalCard(document, card)
    await vi.advanceTimersByTimeAsync(0)
    expect(card.classList.contains(FOCUS_CLASS)).toBe(false)
    await vi.advanceTimersByTimeAsync(550)
    expect(card.classList.contains(FOCUS_CLASS)).toBe(true)
  })
})
