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
  projections: { values: { permissions?: { currentValue: string } } }
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
  create(input: { path: string }): Promise<MockResult<{
    workspace: { workspaceId: string; path: string; title: string }
  }>>
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

interface PermissionPresetsFace {
  catalog(): Promise<MockResult<{
    options: { value: string }[]
    defaultOptions: { value: string }[]
    defaultPreset: string
  }>>
}

interface CommandsFace {
  execute(sessionId: string, line: string): Promise<MockResult<unknown>>
}

const remote = mockRemote() as {
  session: SessionFace
  credentials: CredentialFace
  directoryPicker: DirectoryPickerFace
  workspace: WorkspaceFace
  permissionPresets: PermissionPresetsFace
  commands: CommandsFace
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

describe('mock 添加工作区', () => {
  it('按目录路径建行并广播，默认工作区保留；新会话落在新工作区', async () => {
    const before = await workspaceBaseline()
    const path = 'D:\\MyWorkspace\\SHANIU\\docs'
    const created = await remote.workspace.create({ path })
    if (!created.ok) throw new Error(`创建失败：${created.error.message}`)

    // 标题取目录名；新行进入 follow 基线，默认工作区仍在
    expect(created.value.workspace).toMatchObject({ path, title: 'docs' })
    const after = await workspaceBaseline()
    expect(after).toHaveLength(before.length + 1)
    expect(after.map(row => row.path)).toContain(path)
    expect(after.map(row => row.title)).toContain('default-workspace')

    // 会话挂进新工作区时 cwd 跟着工作区路径走（不传 cwd 的默认路径）
    const session = await remote.session.create({ workspaceId: created.value.workspace.workspaceId })
    const list = await remote.session.list()
    expect(list.value.items.find(item => item.sessionId === session.value.sessionId)?.cwd).toBe(path)
  })
})

describe('mock 权限挡位目录', () => {
  it('目录只含内置挡位与本仓自主档：实验档 auto 不出现，默认表与目录同表', async () => {
    const catalog = await remote.permissionPresets.catalog()
    if (!catalog.ok) throw new Error(`catalog 失败：${catalog.error.message}`)
    // auto（模型自动审查）在 SHANIU 后端没有对应语义，用户裁定不进目录；
    // workspace-write-auto 是本仓扩展档（区内写入与执行不再逐次确认），界面按普通档展示。
    expect(catalog.value.options.map(option => option.value))
      .toEqual(['read-only', 'workspace-write', 'danger-full-access', 'workspace-write-auto'])
    expect(catalog.value.defaultOptions.map(option => option.value))
      .toEqual(catalog.value.options.map(option => option.value))
    expect(catalog.value.defaultPreset).toBe('read-only')
  })

  it('/permission 按目录白名单裁决：挡内切换推投影，auto 回未命中且挡位不动', async () => {
    const created = await remote.session.create()
    const sessionId = created.value.sessionId
    const currentPermission = async (): Promise<string | undefined> => {
      const stream = remote.session.follow({ address: { kind: 'session', sessionId } })
      const snapshot = await stream.next()
      await stream.return?.(undefined)
      return snapshot.value.projections.values.permissions?.currentValue
    }

    const switched = await remote.commands.execute(sessionId, '/permission workspace-write')
    expect(switched).toEqual({ ok: true, value: { matched: true } })
    expect(await currentPermission()).toBe('workspace-write')

    const unmatched = await remote.commands.execute(sessionId, '/permission auto')
    expect(unmatched).toEqual({ ok: true, value: undefined })
    expect(await currentPermission()).toBe('workspace-write')
  })
})

describe('mock 子代理泳道演示（SH-109）', () => {
  /** 泳道 journal 的回放（follow 对 subagent 地址与 session 地址都按 childSessionId 解析）。 */
  async function laneJournal(laneId: string, kind: 'session' | 'subagent' = 'subagent'): Promise<WireEvent[]> {
    const address = kind === 'session'
      ? { kind: 'session' as const, sessionId: laneId }
      : { kind: 'subagent' as const, childSessionId: laneId }
    const stream = (remote.session as unknown as {
      follow(request: { address: { kind: string; sessionId?: string; childSessionId?: string } }): AsyncGenerator<JournalSnapshot>
    }).follow({ address })
    const first = await stream.next()
    await stream.return?.(undefined)
    if (first.done) throw new Error('泳道 follow 缺快照')
    return first.value.records.map(record => record.event)
  }

  /** 控制基线里某会话的投影值（泳道 catalog/timing 的读取通道）。 */
  async function baselineValues(id: string): Promise<Record<string, unknown>> {
    const stream = (remote.session as unknown as {
      control(): AsyncGenerator<{ value: { projections: Record<string, { values: Record<string, unknown> }> } }>
    }).control()
    const first = await stream.next()
    await stream.return?.(undefined)
    if (first.done) throw new Error('control 缺基线')
    return first.value.value.projections[id]?.values ?? {}
  }

  /** 从父会话 journal 的 catalog 事件取泳道 id（run 序号隔离，测试不引用字面量）。 */
  async function laneIdsOfCatalog(parentId: string): Promise<string[]> {
    const stream = (remote.session as unknown as {
      follow(request: { address: { kind: 'session'; sessionId: string } }): AsyncGenerator<JournalSnapshot>
    }).follow({ address: { kind: 'session', sessionId: parentId } })
    const first = await stream.next()
    await stream.return?.(undefined)
    if (first.done) throw new Error('父 follow 缺快照')
    return first.value.records
      .map(record => record.event)
      .filter(event => event.type === 'subagent/catalog')
      .map(event => (event.data as { childId: string }).childId)
  }

  it('/subagent-demo：父落 catalog v3 与汇总，泳道完整回合 + timing 收口', async () => {
    const created = await remote.session.create()
    const parentId = created.value.sessionId
    await send(parentId, '/subagent-demo')
    const events = await eventsAfterTurns(parentId, 1)

    // 父 journal：catalog v3 两条（childId/label/name 齐备）+ 用户命令 + 汇总应答
    const catalogEvents = events.filter(event => event.type === 'subagent/catalog')
    expect(catalogEvents.map(event => event.data)).toEqual([
      expect.objectContaining({ version: 3, mode: 'continuable', name: 'explorer-a', label: '探索 transport 模块边界' }),
      expect.objectContaining({ version: 3, mode: 'continuable', name: 'explorer-b', label: '汇总测试基线' }),
    ])
    // 泳道 id 从 catalog 事件派生（带 run 序号，重复演示不撞 journal）
    const laneIds = catalogEvents.map(event => (event.data as { childId: string }).childId)
    expect(laneIds).toHaveLength(2)
    const summary = events.filter(event => event.type === 'assistant/message').at(-1)
    expect(JSON.stringify(summary?.data)).toContain('两条子代理已完成')

    // 泳道 journal（subagent 地址解析）：descriptor v3 + 完整回合 + 工具调用
    const lane = await laneJournal(laneIds[0]!)
    expect(lane[0]?.type).toBe('subagent/descriptor')
    expect(lane[0]?.data).toMatchObject({ version: 3, mode: 'continuable', provider: 'shaniu-mock', label: '探索 transport 模块边界' })
    expect(lane.map(event => event.type)).toEqual([
      'subagent/descriptor', 'turn/start', 'step/start', 'assistant/message', 'step/end',
      'step/start', 'tool/call', 'tool/result', 'step/end', 'turn/end',
    ])
    expect(lane.at(-1)?.data).toMatchObject({ reason: { kind: 'completed' } })
    // 泳道事件过 wire 闸（assertSessionWireEvent 由客户端 follow 时执行；这里对形状做窄断言）
    expect(lane.every(event => Number.isInteger(event.seq))).toBe(true)

    // session 地址同样能打开泳道（两路地址等价）
    const viaSessionKind = await laneJournal(laneIds[1]!, 'session')
    expect(viaSessionKind[0]?.type).toBe('subagent/descriptor')

    // 控制基线：父挂 subagentCatalog 投影，泳道挂收口后的 subagentTiming
    const parentValues = await baselineValues(parentId)
    expect(parentValues['subagentCatalog']).toEqual([
      expect.objectContaining({ id: laneIds[0], mode: 'continuable', label: '探索 transport 模块边界', name: 'explorer-a' }),
      expect.objectContaining({ id: laneIds[1], mode: 'continuable', label: '汇总测试基线', name: 'explorer-b' }),
    ])
    const laneValues = await baselineValues(laneIds[0]!)
    const timing = laneValues['subagentTiming'] as { settledMs: number; lastTurnCompleted?: boolean; active?: unknown }
    expect(timing.lastTurnCompleted).toBe(true)
    expect(timing.settledMs).toBeGreaterThan(0)
    expect(timing.active).toBeUndefined()
  }, 20_000)

  it('remote.subagents：interrupt 让泳道按 aborted 收口，steering 开续聊回合', async () => {
    const created = await remote.session.create()
    const parentId = created.value.sessionId
    await send(parentId, '/subagent-demo')
    // 泳道 id 从本回合的 catalog 事件派生（run 序号隔离两次演示；catalog 事件在
    // prompt 的同步段就落了，此刻取 id 不会漏）
    const [laneA, laneB] = await laneIdsOfCatalog(parentId)
    // interrupt 必须赶在泳道收口前：explorer-b 错峰 400ms 起步，此刻置中断位，
    // 剧本在首个检查点退出并按 aborted 收口
    const subagents = (remote as unknown as {
      subagents: {
        prompt(request: Record<string, unknown>): Promise<{ ok: boolean; value?: unknown; error?: { code: string } }>
        interruptByUser(childSessionId: string): Promise<{ ok: boolean; value?: unknown }>
      }
    }).subagents
    const interrupted = await subagents.interruptByUser(laneB!)
    expect(interrupted.ok).toBe(true)

    // explorer-a 正常跑完、父代理汇总收口（父回合结束 = 泳道已全部收口）
    await eventsAfterTurns(parentId, 1)
    const abortedEvents = await laneJournal(laneB!)
    const abortedEnds = abortedEvents.filter(event => event.type === 'turn/end')
    expect(abortedEnds).toHaveLength(1)
    expect(abortedEnds[0]?.data).toMatchObject({ reason: { kind: 'aborted' } })

    // 未知泳道如实拒绝
    const missing = await subagents.prompt({ childSessionId: 'nope', content: [] })
    expect(missing.ok).toBe(false)
    expect(missing.error?.code).toBe('subagent/not-found')

    // steering：continuable 泳道（已收口的 explorer-a）开新回合续聊，user/message 带 rpcId
    const steered = await subagents.prompt({
      childSessionId: laneA!, requestId: 'rpc-steer',
      delivery: 'queue', content: [{ type: 'text', text: '补充：注意 settings-face 的边界' }],
    })
    expect(steered.ok).toBe(true)
    const steeredEvents = await laneJournal(laneA!)
    expect(steeredEvents.filter(event => event.type === 'turn/end')).toHaveLength(2)
    expect(steeredEvents.filter(event => event.type === 'user/message').at(-1)?.data)
      .toMatchObject({ source: { kind: 'user', rpcId: 'rpc-steer' } })

    // 已中断的泳道 steering 通道关闭
    const closed = await subagents.prompt({ childSessionId: laneB!, content: [] })
    expect(closed.ok).toBe(false)
    expect(closed.error?.code).toBe('subagent/delivery-unavailable')
  }, 20_000)
})
