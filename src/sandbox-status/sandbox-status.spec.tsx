// @vitest-environment jsdom
/**
 * 沙箱可见面契约测试。
 *
 * 覆盖本卡（SH-021 UI 可见性）承诺的四件事：
 *  1. 插件占的是 `shell.bottom`（常驻）与 `conversation.session.header.utilities`（会话级）；
 *  2. OS 沙箱态如实显示模式 / 机制 / profile，且**不**出现「未启用 OS 沙箱」；
 *  3. 逻辑沙箱态必须出现显眼警告与不同的数据属性（视觉分档由 CSS 承担）；
 *  4. 会话头部的批准与撤销各走一次宿主命令，并把结果同时写进两处可见面。
 *
 * 真实出站是否真的被放行/切断，由真机探针 `network_grant_allows_curl_and_revoke_cuts_it_off`
 * 验证，不在这里假装断言。
 */
import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { fireEvent, render, screen, waitFor } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as sandboxApply, inject as sandboxInject } from './index'
import { SandboxStatusBar } from './SandboxStatusBar'
import { NetworkToggle } from './NetworkToggle'
import { zh } from './locales'
import { getState, refreshGrants, setNotice } from './state'
import type { NetworkGrantWire, SandboxStatusWire, SandboxWire } from './wire'

const SESSION = 'sess-0123456789'

/** 词典直译（测试只关心键是否被用到，不关心八语种）。 */
function translate(key: string, params?: Record<string, unknown>): string {
  const template = (zh as Record<string, string>)[key] ?? key
  return template.replace(/\{(\w+)\}/g, (_match, name: string) => String(params?.[name] ?? `{${name}}`))
}

const OS_STATUS: SandboxStatusWire = {
  mode: 'workspace_write',
  os: 'app_container',
  os_enabled: true,
  profile: 'Shaniu.Sandbox.v1',
  note: 'AppContainer 路径/读隔离 + 网络默认拒绝；受限令牌与 Job 作为纵深保留',
}

const LOGICAL_STATUS: SandboxStatusWire = {
  mode: 'workspace_write',
  os: 'logical',
  os_enabled: false,
  profile: '',
  note: '当前未启用 OS 沙箱：仅受限令牌 + Job（逻辑隔离），没有路径级写入边界',
}

interface FakeWire extends SandboxWire {
  readonly granted: string[]
  readonly revoked: string[]
}

/** 脚本化的数据面：记录调用并维护一份内存授权表。 */
function fakeWire(options: {
  status?: SandboxStatusWire
  grants?: readonly NetworkGrantWire[]
  revokeResult?: boolean
  grantsError?: string
}): FakeWire {
  const granted: string[] = []
  const revoked: string[] = []
  let grants = [...(options.grants ?? [])]
  return {
    granted,
    revoked,
    status: async () => options.status ?? OS_STATUS,
    grants: async () => {
      if (options.grantsError !== undefined) throw new Error(options.grantsError)
      return grants
    },
    grant: async (sessionId: string) => {
      granted.push(sessionId)
      const grant: NetworkGrantWire = { session_id: sessionId, granted_by: 'tester', granted_at_ms: 1, scope: 'test' }
      grants = [...grants, grant]
      return grant
    },
    revoke: async (sessionId: string) => {
      revoked.push(sessionId)
      grants = grants.filter((grant) => grant.session_id !== sessionId)
      return options.revokeResult ?? true
    },
  }
}

/** 槽位标准套件里的其余 hook 与本用例无关：按渲染契约只喂它真正用到的几件。 */
function barElement(wire: SandboxWire): ReactNode {
  const Bar = SandboxStatusBar as unknown as (props: Record<string, unknown>) => ReactNode
  return <Bar t={translate} wire={wire} />
}

function toggleElement(wire: SandboxWire, sessionId: string = SESSION): ReactNode {
  const Toggle = NetworkToggle as unknown as (props: Record<string, unknown>) => ReactNode
  return <Toggle sessionId={sessionId} t={translate} wire={wire} />
}

beforeEach(async () => {
  vi.restoreAllMocks()
  setNotice(null)
  await refreshGrants(fakeWire({ grants: [] }))
})

describe('src/sandbox-status：插件接缝', () => {
  it('只依赖 slots 与 locale，同时占常驻状态条与会话头部开关', () => {
    expect(sandboxInject).toEqual(['slots', 'locale'])

    const waited: string[] = []
    const ctx = new Context()
    ctx.provide('locale', { register: () => () => {}, bind: () => translate } as never)
    ctx.provide('slots', {
      inject: (key: string) => { waited.push(key); return () => {} },
      register: () => () => {},
    } as never)

    sandboxApply(ctx)
    expect(waited).toEqual(['shell.bottom', 'conversation.session.header.utilities'])
  })
})

describe('src/sandbox-status：常驻状态呈现', () => {
  it('OS 沙箱态显示 AppContainer 与 profile，且没有任何"未启用"警告', async () => {
    render(barElement(fakeWire({ status: OS_STATUS })))
    await screen.findByText('OS 沙箱（AppContainer）')
    expect(screen.getByText('Shaniu.Sandbox.v1')).toBeTruthy()
    expect(screen.getByText('工作区可写')).toBeTruthy()
    expect(screen.getByText(zh['network.denied'])).toBeTruthy()
    expect(document.body.textContent).not.toContain('当前未启用 OS 沙箱')
    expect(document.querySelector('.shaniu-sandbox-bar')?.getAttribute('data-os')).toBe('os')
  })

  it('逻辑沙箱态必须显眼：警告文字 + 不同的 data-os', async () => {
    render(barElement(fakeWire({ status: LOGICAL_STATUS })))
    await screen.findByText('逻辑沙箱')
    const warning = screen.getByText(zh['os.logicalWarning'])
    expect(warning.closest('.shaniu-sandbox-bar-warning')).toBeTruthy()
    expect(document.querySelector('.shaniu-sandbox-bar')?.getAttribute('data-os')).toBe('logical')
    expect(document.body.textContent).not.toContain('Shaniu.Sandbox.v1')
    // 网络在逻辑沙箱下没有 OS 拦截：状态条不能说「已断（默认拒绝）」。
    expect(screen.getByText(zh['network.notEffective'])).toBeTruthy()
  })

  it('已放行时状态条列出被放行的会话，撤销后从列表消失', async () => {
    const wire = fakeWire({
      status: OS_STATUS,
      grants: [{ session_id: SESSION, granted_by: 'tester', granted_at_ms: 1, scope: 'test' }],
    })
    render(barElement(wire))
    await screen.findByText(/^已放行：sess-0123/)

    await wire.revoke(SESSION)
    await refreshGrants(wire)
    await waitFor(() => expect(screen.getByText(zh['network.denied'])).toBeTruthy())
  })

  it('授权列表拉取失败要变成可见反馈，而不是假装"没放行"', async () => {
    render(barElement(fakeWire({ status: OS_STATUS, grantsError: 'audit unavailable' })))
    await screen.findByText(/audit unavailable/)
  })
})

describe('src/sandbox-status：会话级网络开关', () => {
  it('未放行时点击即批准本会话，并把反馈写进共享状态', async () => {
    const wire = fakeWire({ status: OS_STATUS })
    render(toggleElement(wire))

    fireEvent.click(screen.getByRole('button'))

    await waitFor(() => expect(wire.granted).toEqual([SESSION]))
    await waitFor(() => expect(getState().grants.map((g) => g.session_id)).toEqual([SESSION]))
    expect(getState().notice?.text).toBe(zh['feedback.granted'])
  })

  it('已放行时点击即撤销，反馈写明后续命令将断网', async () => {
    const wire = fakeWire({
      status: OS_STATUS,
      grants: [{ session_id: SESSION, granted_by: 'tester', granted_at_ms: 1, scope: 'test' }],
    })
    await refreshGrants(wire)
    render(toggleElement(wire))
    await screen.findByLabelText(zh['action.revokeTooltip'])

    fireEvent.click(screen.getByRole('button'))

    await waitFor(() => expect(wire.revoked).toEqual([SESSION]))
    await waitFor(() => expect(getState().grants).toEqual([]))
    expect(getState().notice?.text).toBe(zh['feedback.revoked'])
  })

  it('逻辑沙箱下开关显示「未生效」并禁用，点击不产生授权', async () => {
    const wire = fakeWire({ status: LOGICAL_STATUS })
    render(toggleElement(wire))
    await screen.findByLabelText(zh['network.notEffective'])

    const button = screen.getByRole('button') as HTMLButtonElement
    expect(button.disabled).toBe(true)
    fireEvent.click(button)
    expect(wire.granted).toEqual([])
  })

  it('后端说"本来就没有授权"时不假装撤销成功', async () => {
    const wire = fakeWire({
      status: OS_STATUS,
      grants: [{ session_id: SESSION, granted_by: 'tester', granted_at_ms: 1, scope: 'test' }],
      revokeResult: false,
    })
    await refreshGrants(wire)
    render(toggleElement(wire))
    await screen.findByLabelText(zh['action.revokeTooltip'])

    fireEvent.click(screen.getByRole('button'))

    await waitFor(() => expect(getState().notice?.text).toBe(zh['feedback.notGranted']))
  })
})
