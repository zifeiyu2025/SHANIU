import { describe, expect, it, vi } from 'vitest'

const opened = vi.fn(async (_path: string) => true)
vi.mock('./open-workspace', () => ({
  openInSystemExplorer: (path: string) => opened(path),
}))

import { apply } from './index'
import { sessionToMarkdown } from './session-markdown'

interface Registration {
  options: Record<string, unknown>
  component: unknown
}

interface MenuAction {
  id: string
  run?: () => void | Promise<void>
}

interface Contribution {
  id: string
  target: string
  resolve: (target: { sessionId: string }, signal: AbortSignal) => readonly MenuAction[]
}

interface ShortcutCommand {
  id: string
  defaults: Record<string, { code: string; modifiers: readonly string[] }>
  resolve: () => { status: string; reason?: string; run?: () => void }
}

const SESSION_ID = 'mock-session-demo'
const WORKSPACE = 'D:\\MyWorkspace\\SHANIU'

const NODES = [
  { kind: 'user', seq: 0, time: 0, source: {}, content: [{ type: 'text', text: '梳理一下 mock' }] },
  {
    kind: 'assistant',
    seq: 1,
    time: 0,
    turn: 0,
    step: 0,
    blocks: [
      { kind: 'reasoning', text: '先看目录' },
      { kind: 'tool-call', callId: 'c1', name: 'grep', argsRaw: '{"pattern":"mock"}' },
      { kind: 'text', text: '结论：**两条线**' },
    ],
  },
]

/** 记录注册结果的最小 ctx；会话行可按需覆盖（缺 cwd / 无主视图会话）。 */
function harness(row: Record<string, unknown> | null = { id: SESSION_ID, cwd: WORKSPACE, displayTitle: '多步演示', retainedBy: { mainView: 1 } }) {
  const registrations: Registration[] = []
  const contributions: Contribution[] = []
  const commands: ShortcutCommand[] = []
  const ctx = {
    effect: (fn: () => unknown) => fn(),
    logger: { warn: () => {} },
    locale: { register: () => () => {}, bind: () => (key: string, params?: { app?: string }) => params?.app === undefined ? key : `${key}:${params.app}` },
    slots: {
      inject: (_name: string, callback: () => () => void) => {
        callback()
        return () => {}
      },
      register: (options: Record<string, unknown>, component: unknown) => {
        registrations.push({ options, component })
        return () => {}
      },
    },
    contextMenus: {
      register: (contribution: Contribution) => {
        contributions.push(contribution)
        return () => {}
      },
    },
    shortcuts: { register: (command: ShortcutCommand) => { commands.push(command); return () => {} }, catalog: { getSnapshot: () => [], subscribe: () => () => {} } },
    sessions: { list: { getSnapshot: () => ({ byId: row === null ? {} : { [SESSION_ID]: row } }) } },
    uiConversation: { binding: () => ({ target: () => ({ getSnapshot: () => ({ legacy: { nodes: NODES } }) }) }) },
  }
  apply(ctx as never)
  return { registrations, contributions, commands }
}

/** 取出 utilities 槽位注册项，按会话拿到它的 open 回调。 */
function explorerOpen(registrations: readonly Registration[], sessionId = SESSION_ID): () => void {
  const entry = registrations.find(item => item.options.name === 'conversation.session.header.utilities')
  if (entry === undefined) throw new Error('utilities entry not registered')
  expect(entry.options.id).toBe('shaniu-explorer')
  expect(entry.options.order).toBe(-10)
  return (entry.options.inject as (id: string) => { open: () => void })(sessionId).open
}

/** 取出 sessionHeader 目标的菜单贡献并解析一次。 */
function sessionMenu(contributions: readonly Contribution[], sessionId = SESSION_ID): readonly MenuAction[] {
  const contribution = contributions.find(item => item.target === 'sessionHeader')
  if (contribution === undefined) throw new Error('sessionHeader contribution not registered')
  return contribution.resolve({ sessionId }, new AbortController().signal)
}

describe('header actions', () => {
  it('资源管理器按钮直接打开会话的工作区目录', async () => {
    const { registrations } = harness()
    explorerOpen(registrations)()
    await vi.waitFor(() => expect(opened).toHaveBeenCalledWith(WORKSPACE))
  })

  it('会话没有 cwd 时不做任何事', async () => {
    const { registrations } = harness({ id: SESSION_ID, displayTitle: '无目录', retainedBy: { mainView: 1 } })
    explorerOpen(registrations)()
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(opened).not.toHaveBeenCalled()
  })

  it('注册 workspace.openLocal：桌面 Ctrl+Alt+O，作用在当前主视图会话上', async () => {
    const { commands, registrations } = harness()
    void registrations
    const command = commands.find(item => item.id === 'workspace.openLocal')
    expect(command?.defaults['desktop:windows']).toEqual({ code: 'KeyO', modifiers: ['primary', 'alt'] })
    expect(command?.defaults['web:windows']).toEqual({ code: 'KeyO', modifiers: ['primary', 'shift'] })
    const resolution = command?.resolve()
    expect(resolution?.status).toBe('handled')
    resolution?.run?.()
    await vi.waitFor(() => expect(opened).toHaveBeenCalledWith(WORKSPACE))
  })

  it('没有主视图会话时快捷键给出阻塞原因', () => {
    const { commands } = harness(null)
    const resolution = commands.find(item => item.id === 'workspace.openLocal')?.resolve()
    expect(resolution?.status).toBe('blocked')
    expect(resolution?.reason).toBe('explorer.noSession')
  })

  it('更多操作菜单提供复制 Markdown 与复制会话 ID', async () => {
    const { contributions } = harness()
    const actions = sessionMenu(contributions)
    expect(actions.map(action => action.id)).toEqual(['copy-markdown', 'copy-id'])

    const writeText = vi.fn(async () => {})
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })

    await actions[0]?.run?.()
    expect(writeText).toHaveBeenLastCalledWith([
      '# 多步演示',
      '',
      '## 用户',
      '',
      '梳理一下 mock',
      '',
      '## 助手',
      '',
      '> 先看目录',
      '',
      '- 调用 `grep`：{"pattern":"mock"}',
      '',
      '结论：**两条线**',
      '',
    ].join('\n'))

    await actions[1]?.run?.()
    expect(writeText).toHaveBeenLastCalledWith(SESSION_ID)
  })
})

describe('sessionToMarkdown', () => {
  it('只保留文本、思考与工具调用行', () => {
    const markdown = sessionToMarkdown('标题', NODES as never)
    expect(markdown.startsWith('# 标题')).toBe(true)
    expect(markdown).toContain('> 先看目录')
    expect(markdown).toContain('- 调用 `grep`')
  })

  it('空会话只剩标题', () => {
    expect(sessionToMarkdown('空', [])).toBe('# 空\n')
  })
})
