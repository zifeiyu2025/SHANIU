import { describe, expect, it } from 'vitest'
import { mockRemote, setDefaultWorkspacePath } from './mock'

/** 只声明本文件用到的几个面，其余成员按 mock 的实际形状留着。 */
interface WireEvent {
  type: string
  seq: number
  data: Record<string, unknown>
}

interface JournalSnapshot {
  records: { type: string; event: WireEvent }[]
}

interface SessionFace {
  create(payload?: { workspaceId?: string; cwd?: string }): Promise<{ ok: true; value: { sessionId: string } }>
  prompt(request: { requestId?: string; sessionId?: string; content: readonly unknown[] }): Promise<unknown>
  selectModel(selection: Record<string, unknown>): Promise<unknown>
  follow(request: { address: { kind: 'session'; sessionId: string } }): AsyncGenerator<JournalSnapshot>
  list(): Promise<{ ok: true; value: { items: { sessionId: string; cwd: string }[] } }>
}

interface WorkspaceFace {
  follow(request: unknown): AsyncGenerator<{
    type: string
    value: { items: { path: string; title: string }[] }
  }>
}

interface CredentialFace {
  set(ref: string, value: string): Promise<unknown>
  unset(ref: string): Promise<unknown>
}

type MockResult<T> = { ok: true; value: T } | { ok: false; error: { code: string; message: string } }

interface DirectoryEntry {
  name: string
  path: string
  hidden: boolean
}

interface DirectoryListing {
  path: string
  home: string
  crumbs: DirectoryEntry[]
  entries: DirectoryEntry[]
  truncated: boolean
}

interface DirectoryPickerFace {
  list(path?: string): Promise<MockResult<DirectoryListing>>
  createDirectory(path: string, name: string): Promise<MockResult<string>>
}

const remote = mockRemote() as {
  session: SessionFace
  credentials: CredentialFace
  directoryPicker: DirectoryPickerFace
  workspace: WorkspaceFace
}

/** 读工作区 follow 的第一帧（baseline 就是全集）。 */
async function workspaceBaseline(): Promise<{ path: string; title: string }[]> {
  const stream = remote.workspace.follow({})
  const first = await stream.next()
  await stream.return?.(undefined)
  if (first.done) throw new Error('workspace follow 缺少 baseline')
  return first.value.value.items
}

/**
 * 回放一个会话的 journal，直到已提交 `turnEnds` 个回合为止；
 * 成功回合带流式分块，需要等，故按轮次计数而不是"看到一条就算"。
 */
async function eventsAfterTurns(sessionId: string, turnEnds: number): Promise<WireEvent[]> {
  for (let attempt = 0; attempt < 200; attempt += 1) {
    const stream = remote.session.follow({ address: { kind: 'session', sessionId } })
    const first = await stream.next()
    await stream.return?.(undefined)
    if (first.done !== true) {
      const events = first.value.records.map(record => record.event)
      if (events.filter(event => event.type === 'turn/end').length >= turnEnds) return events
    }
    await new Promise<void>(resolve => { setTimeout(resolve, 20) })
  }
  throw new Error('回合没有在预期时间内结束')
}

const send = async (sessionId: string, text: string): Promise<void> => {
  await remote.session.prompt({ sessionId, requestId: `rpc-${text}`, content: [{ type: 'text', text }] })
}

describe('mock 回合剧本与模型配置状态一致', () => {
  it('没配任何服务商时发送只报错，不产出助手回答', async () => {
    const created = await remote.session.create()
    const sessionId = created.value.sessionId
    await send(sessionId, '未配置时发一条')
    const events = await eventsAfterTurns(sessionId, 1)

    expect(events.filter(event => event.type === 'assistant/message')).toEqual([])
    expect(events.find(event => event.type === 'turn/end')?.data).toMatchObject({
      reason: { kind: 'error', error: { code: 'NO_MODEL_CONFIGURED' } },
    })
  })

  it('配好服务商后同一会话走成功回合，失败回合照样占一个回合号', async () => {
    const created = await remote.session.create()
    const sessionId = created.value.sessionId
    try {
      await send(sessionId, '先失败一轮')
      const firstTurn = await eventsAfterTurns(sessionId, 1)
      expect(firstTurn.filter(event => event.type === 'turn/end')[0]?.data).toMatchObject({
        turn: 0,
        reason: { kind: 'error' },
      })

      await remote.credentials.set('DEEPSEEK_API_KEY', 'mock-verify-key')
      await remote.session.selectModel({ provider: 'deepseek', model: 'deepseek-v4-flash' })
      await send(sessionId, '配好了再发')
      const events = await eventsAfterTurns(sessionId, 2)

      // 回合号必须接着往下走：写死 0 会让第二轮落在已关闭的回合上，过程头不出现
      expect(events.filter(event => event.type === 'turn/end')[1]?.data).toMatchObject({
        turn: 1,
        reason: { kind: 'completed' },
      })
      const usage = events.filter(event => event.type === 'assistant/message')[0]?.data.usage
      expect(usage).toMatchObject({ inputTokens: 9600, outputTokens: 300, totalTokens: 9900 })
    } finally {
      await remote.credentials.unset('DEEPSEEK_API_KEY')
    }
  })
})

describe('mock 目录选择器', () => {
  it('列主目录：面包屑从 home 起，条目按名排序且路径由宿主拼好', async () => {
    const result = await remote.directoryPicker.list()
    if (!result.ok) throw new Error(`list 失败：${result.error.message}`)
    const listing = result.value

    expect(listing.path).toBe(listing.home)
    expect(listing.crumbs).toEqual([{ name: listing.home, path: listing.home, hidden: false }])
    expect(listing.entries.map(entry => entry.name)).toEqual([...listing.entries.map(entry => entry.name)].sort())
    expect(listing.entries.find(entry => entry.name === 'src')?.path).toBe(`${listing.home}\\src`)
  })

  it('列子目录：面包含缀出 home → 子目录，尾部斜杠不影响结果', async () => {
    const home = await remote.directoryPicker.list()
    if (!home.ok) throw new Error('list 失败')
    const nested = await remote.directoryPicker.list(`${home.value.path}\\src\\`)
    if (!nested.ok) throw new Error(`list 失败：${nested.error.message}`)

    expect(nested.value.path).toBe(`${home.value.home}\\src`)
    expect(nested.value.crumbs.map(crumb => crumb.name)).toEqual([home.value.home, 'src'])
  })

  it('新建目录立即可列；重名按 exists 拒绝', async () => {
    const home = await remote.directoryPicker.list()
    if (!home.ok) throw new Error('list 失败')
    const created = await remote.directoryPicker.createDirectory(home.value.path, 'mock-new-dir')
    expect(created).toEqual({ ok: true, value: `${home.value.home}\\mock-new-dir` })

    const after = await remote.directoryPicker.list(home.value.path)
    if (!after.ok) throw new Error('list 失败')
    expect(after.value.entries.map(entry => entry.name)).toContain('mock-new-dir')

    const duplicate = await remote.directoryPicker.createDirectory(home.value.path, 'mock-new-dir')
    expect(duplicate.ok ? 'unexpected ok' : duplicate.error.code).toBe('directory-picker/exists')
  })
})

describe('mock 默认工作区路径', () => {
  it('桌面壳可把默认工作区指到宿主解析出的 <数据根>\\Workspace，新会话 cwd 跟随', async () => {
    const [before] = await workspaceBaseline()
    const target = 'D:\\data\\Workspace'
    try {
      setDefaultWorkspacePath(target)
      const [after] = await workspaceBaseline()
      // 标题保持自动名（界面据此渲染「默认工作区」），路径换成宿主给的
      expect(after).toMatchObject({ path: target, title: 'default-workspace' })

      const created = await remote.session.create()
      const list = await remote.session.list()
      expect(list.value.items.find(item => item.sessionId === created.value.sessionId)?.cwd).toBe(target)
    } finally {
      if (before !== undefined) setDefaultWorkspacePath(before.path)
    }
  })
})
