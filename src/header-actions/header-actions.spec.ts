import { afterEach, describe, expect, it, vi } from 'vitest'

/**
 * 资源管理器走**真实** `openInSystemExplorer`：它动态 import `@tauri-apps/api/core`，
 * 后者调用 `window.__TAURI_INTERNALS__.invoke`。这里只装宿主对象、不打桩业务模块——
 * 要证的正是"点下去真的产生了 invoke"（产物回归丢的就是这一步）。
 *
 * 宿主对象**按用例装**：`inTauri()` 同时决定剪贴板走宿主命令还是 navigator，
 * 全局留着会改变导出/复制用例的路径。
 */
const hostInvoke = vi.hoisted(() => vi.fn(async (_cmd: string, _args?: Record<string, unknown>): Promise<unknown> => undefined))

function installHost(): void {
  Object.defineProperty(window, '__TAURI_INTERNALS__', {
    value: { invoke: hostInvoke, transformCallback: (callback: unknown) => callback },
    configurable: true,
  })
}

afterEach(() => { Reflect.deleteProperty(window, '__TAURI_INTERNALS__') })

/** `load_session` 的替身：Markdown 复制的唯一宿主调用。 */
const invoke = vi.hoisted(() => vi.fn(async (): Promise<unknown> => []))
vi.mock('../transport/tauri', () => ({
  tauriBridge: () => ({ invoke, listenEvents: () => () => {} }),
}))

import { apply } from './index'
import { formatSessionMarkdown } from '../dsh/packages/session-query/session-log-export/src/markdown'
import { sessionEntriesToEvents } from './session-events'

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

/**
 * `load_session` 的真实返回形状（`SessionEntry`）：一个用户回合里
 * 一次工具调用 + 一条最终回复。用它钉住"档案条目 → 上游事件 → Markdown"整条链。
 */
const ENTRIES = [
  { kind: 'message', role: 'user', text: '梳理一下 mock' },
  { kind: 'tool_call', call_id: 'c1', tool: 'grep', arguments: '{"pattern":"mock"}', output: 'a.ts:1' },
  { kind: 'message', role: 'assistant', text: '结论：**两条线**' },
]

/** 记录注册结果的最小 ctx；会话行可按需覆盖（缺 cwd / 无主视图会话）。 */
function harness(row: Record<string, unknown> | null = { id: SESSION_ID, cwd: WORKSPACE, displayTitle: '多步演示', retainedBy: { mainView: 1 } }) {
  const registrations: Registration[] = []
  const contributions: Contribution[] = []
  const commands: ShortcutCommand[] = []
  const warnings: unknown[][] = []
  const ctx = {
    effect: (fn: () => unknown) => fn(),
    logger: { warn: (...args: unknown[]) => { warnings.push(args) } },
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
    sessions: {
      list: {
        getSnapshot: () => ({
          ids: row === null ? [] : [SESSION_ID],
          byId: row === null ? {} : { [SESSION_ID]: row },
          phase: 'ready',
        }),
      },
    },
    uiConversation: { binding: () => ({ target: () => ({ getSnapshot: () => ({ legacy: { nodes: [] } }) }) }) },
  }
  apply(ctx as never)
  return { registrations, contributions, commands, warnings }
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
  it('资源管理器按钮直接打开会话的工作区目录：有会话、有 cwd 就必然产生 invoke', async () => {
    installHost()
    hostInvoke.mockClear()
    const { registrations, warnings } = harness()
    explorerOpen(registrations)()
    await vi.waitFor(() => expect(hostInvoke).toHaveBeenCalled())
    // 上游 invoke 会带第三个 options 形参，这里只看命令与载荷。
    expect(hostInvoke.mock.calls[0]?.slice(0, 2)).toEqual(['open_in_explorer', { path: WORKSPACE }])
    expect(warnings).toEqual([])
  })

  it('列表行缺 cwd 时不静默：warn 点名会话与缺什么，且不发 invoke', async () => {
    // 反向对照：修之前这里是裸 return——产物回归里表现为"点按钮毫无反应、console 也没有
    // 任何输出"，排查时只能靠人肉反编译。现在必须留下可读的一条。
    installHost()
    hostInvoke.mockClear()
    const { registrations, warnings } = harness({ id: SESSION_ID, displayTitle: '无目录', retainedBy: { mainView: 1 } })
    explorerOpen(registrations)()
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(hostInvoke).not.toHaveBeenCalled()
    expect(warnings).toHaveLength(1)
    expect(warnings[0]?.slice(1)).toEqual([SESSION_ID, '缺 cwd', 'ready', 1])
  })

  it('会话根本不在列表快照里时也要留痕（phase 与条数进日志）', async () => {
    installHost()
    hostInvoke.mockClear()
    const { registrations, warnings } = harness(null)
    explorerOpen(registrations)()
    await new Promise(resolve => setTimeout(resolve, 0))
    expect(hostInvoke).not.toHaveBeenCalled()
    expect(warnings).toHaveLength(1)
    expect(warnings[0]?.slice(1)).toEqual([SESSION_ID, '不存在', 'ready', 0])
  })

  it('不重复注册 workspace.openLocal：命令位归官方 ui-open-in-app', () => {
    // 同 id 二次注册会让 ShortcutsRegistry 抛错并回滚整颗 fiber——曾经因此让
    // 资源管理器按钮与「更多操作」三项一起消失（见 boot 级装载健康测试）。
    const { commands } = harness()
    expect(commands.some(item => item.id === 'workspace.openLocal')).toBe(false)
  })

  it('复制 Markdown 走宿主全量档案，而不是界面已加载的节点', async () => {
    invoke.mockResolvedValue(ENTRIES)
    const { contributions } = harness()
    const actions = sessionMenu(contributions)
    const writeText = vi.fn(async (_text: string) => {})
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })

    await actions.find(action => action.id === 'copy-markdown')?.run?.()

    // 取数走 `load_session`（整份档案），不读 UI 快照——这是"未加载历史也能导出"的落点。
    expect(invoke).toHaveBeenCalledWith('load_session', { agentId: SESSION_ID })
    const markdown = String(writeText.mock.calls[0]?.[0] ?? '')
    expect(markdown.startsWith('# 多步演示')).toBe(true)
    expect(markdown).toContain('> 梳理一下 mock')
    // 工具活动折进 details，最终回复留在正文（上游 formatter 的分段规则）
    expect(markdown).toContain('<details>')
    expect(markdown).toContain('**Tool: `grep`**')
    expect(markdown).toContain('结论：**两条线**')
    // 单一尾换行是上游契约
    expect(markdown.endsWith('\n')).toBe(true)
    expect(markdown.endsWith('\n\n')).toBe(false)
  })

  it('复制会话 ID 仍走剪贴板且不触发宿主读取', async () => {
    invoke.mockClear()
    const { contributions } = harness()
    const actions = sessionMenu(contributions)
    const writeText = vi.fn(async (_text: string) => {})
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true })

    await actions.find(action => action.id === 'copy-id')?.run?.()
    expect(writeText).toHaveBeenLastCalledWith(SESSION_ID)
    expect(invoke).not.toHaveBeenCalled()
  })

  it('更多操作三项入口（复制 Markdown / 下载日志 / 复制 ID），不出现双份「复制 Markdown」', () => {
    const { contributions } = harness()
    // 顺序即渲染顺序：上游是 copy → download，本地补充项排在最后。
    expect(sessionMenu(contributions).map(action => action.id))
      .toEqual(['copy-markdown', 'download', 'copy-id'])
  })
})

describe('Markdown 导出（上游 formatter 的语义契约）', () => {
  it('多回合：用户消息 quote、早期工具活动折 details、最终回复留在正文', () => {
    const entries = [
      { kind: 'message', role: 'user', text: '梳理一下 mock' },
      { kind: 'tool_call', call_id: 'c1', tool: 'grep', arguments: '{"pattern":"mock"}', output: 'a.ts:1' },
      { kind: 'message', role: 'assistant', text: '先看一下目录结构。' },
      { kind: 'message', role: 'user', text: '第二条任务' },
      { kind: 'message', role: 'assistant', text: '```ts\n未闭合的代码' },
    ] as const
    const markdown = formatSessionMarkdown(
      sessionEntriesToEvents(entries as never, '多步演示'),
    )
    // 整份输出逐字钉住：分段、折叠、JSON 参数格式化、未闭合栅栏补闭、单一尾换行
    // 都是上游 `formatSessionMarkdown` 的可见契约，任何一处漂移都应让这条红。
    expect(markdown).toBe([
      '# 多步演示',
      '',
      '> 梳理一下 mock',
      '',
      '<details><summary>1 previous message</summary>',
      '',
      '**Tool: `grep`**',
      '',
      '```json',
      '{',
      '  "pattern": "mock"',
      '}',
      '```',
      '',
      '**Result**',
      '',
      '```',
      'a.ts:1',
      '```',
      '',
      '</details>',
      '',
      '先看一下目录结构。',
      '',
      '> 第二条任务',
      '',
      '```ts',
      '未闭合的代码',
      '```',
      '',
    ].join('\n'))
  })

  it('空档案只剩标题，且只有一个尾换行', () => {
    expect(formatSessionMarkdown(sessionEntriesToEvents([], '空会话'))).toBe('# 空会话\n')
  })
})
