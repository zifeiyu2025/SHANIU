/**
 * SH-017 适配层契约测试：判据是**行为与契约**，不是整段 JSON 字面量快照。
 *
 * 这里刻意复用 DSH 自己的 wire 校验器（`assertSessionWireEvent`）——适配层的输出
 * 必须过客户端运行时真正会过的那道闸，而不是我在测试里另写一份"看起来一样"的规则。
 */
import { describe, expect, it, vi } from 'vitest'
import { assertSessionWireEvent } from '../dsh/packages/api/session-controller/src/client/session-wire-event.ts'
import { ClientWorkspaceModel } from '../dsh/packages/api/workspace-controller/src/client/model.ts'
import { createAdapter } from './adapter'
import { registerExportTrigger } from './command-triggers'
import type { HostBridge } from './tauri'
import type { AgentEvent, EventMsg } from './types'

/** 宿主 `get_settings` 的真实形状（04 §1.3）：服务商文档 + 逐家 hasApiKey。 */
const HOST_SETTINGS = {
  baseUrl: 'https://lc/v1',
  model: 'LongCat-2.5',
  maxTurns: 32,
  workspaceRoot: 'W',
  hasApiKey: true,
  sandboxMode: 'workspace_write',
  approvalMode: 'ask',
  activeProvider: 'longcat',
  providerRevision: 4,
  providers: [
    {
      id: 'longcat',
      label: 'LongCat',
      provider: 'openai-chat',
      baseUrl: 'https://lc/v1',
      model: 'LongCat-2.5',
      secretRef: 'LONGCAT_API_KEY',
      hasApiKey: true,
      models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', contextWindow: 128_000, maxTokens: 8_192, input: ['text'] }],
    },
    {
      id: 'ghost',
      label: 'Ghost',
      provider: 'openai-chat',
      baseUrl: 'https://g/v1',
      model: 'g-1',
      secretRef: 'GHOST_API_KEY',
      hasApiKey: false,
      models: [],
    },
  ],
}

interface ScriptedBridge extends HostBridge {
  readonly calls: { command: string; args: Record<string, unknown> | undefined }[]
  emit(sessionId: string, msg: EventMsg, agentId?: string): void
}

function scriptedBridge(
  settings: Record<string, unknown> = HOST_SETTINGS,
  archive: Record<string, unknown>[] = [],
  renameFailure?: unknown,
  saveFailure?: unknown,
  /** SH-113：`search_sessions` 的宿主替身（可 throw {code,message} 模拟类型化错误）。 */
  searchHandler?: (args: Record<string, unknown> | undefined) => unknown,
  /** SH-115：`compact_session` 的宿主替身（null = 没有可压历史；可 throw 模拟失败）。 */
  compactHandler?: (args: Record<string, unknown> | undefined) => unknown,
  /** SH-092：`workspace_file_*` 四条命令的宿主替身（throw {code,message,details} = 类型化失败）。 */
  fileHandler?: (command: string, args: Record<string, unknown> | undefined) => unknown,
  /** SH-092 批 3：open-in-app 的 `workspace_path_applications`/`open_workspace_path` 替身。 */
  openHandler?: (command: string, args: Record<string, unknown> | undefined) => unknown,
  /** SH-116：`read_attachment` 替身（缺省回一个文本文件）。 */
  attachmentRead?: (args: Record<string, unknown> | undefined) => unknown,
  /** SH-089：`job_list`/`job_read`/`job_kill` 的宿主替身（throw {code,message} = 类型化失败）。 */
  jobHandler?: (command: string, args: Record<string, unknown> | undefined) => unknown,
): ScriptedBridge {
  const calls: ScriptedBridge['calls'] = []
  const handlers = new Set<(event: AgentEvent) => void>()
  let counter = 0
  // SH-100 的侧车状态（每份桥独立克隆，避免测试间串味）
  const workspaceFlags = { archived: [] as string[], pinned: [] as string[], order: [] as string[] }
  // SH-115：进程级 PlanState 的替身（set/get 共用同一份布尔，模拟宿主持久态）
  let planActive = false
  // SH-088：目标 durable 投影的替身（revision CAS 语义对齐 goals.rs）
  let goalState: Record<string, unknown> | null = null
  let goalSeq = 0
  // 宿主语义的关键一条：save_settings 落盘 config.toml，之后的 get_settings 回读新值。
  // 测试桥按同一语义模拟（每份桥独立的克隆，避免测试间串味），挡位切换的投影推帧
  // 才能测到"切换后 UI 显示新挡位"而不是停在旧值。
  const state = structuredClone(settings)
  return {
    calls,
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      calls.push({ command, args })
      if (command === 'get_settings') return state as T
      if (command === 'workspace_path_applications' || command === 'open_workspace_path' || command === 'list_file_references' || command === 'list_session_references') {
        if (openHandler === undefined) return (command === 'open_workspace_path' ? undefined : []) as T
        return openHandler(command, args) as T
      }
      if (command.startsWith('workspace_file_')) {
        // 缺省给一份最小可用形状：单测只在需要断言失败/内容时才接 fileHandler。
        if (fileHandler === undefined) return { absolutePath: 'C:\w\a.txt', version: '1-2', bytes: 2 } as T
        return fileHandler(command, args) as T
      }
      // SH-115：计划态命令面的替身
      if (command === 'set_plan_mode') {
        planActive = args?.['active'] === true
        return planActive as T
      }
      if (command === 'get_plan_mode') return planActive as T
      if (command === 'compact_session') {
        if (compactHandler === undefined) return null as T
        return compactHandler(args) as T
      }
      // SH-088：目标 store 的宿主替身——与 goals.rs 同语义：稳定 id、单调 revision、
      // 所有写操作做 CAS；activation 不在宿主，只回 durable 投影。
      if (command === 'goal_get') return goalState as T
      if (command === 'goal_create') {
        const phase = (goalState?.['goal'] as Record<string, unknown> | undefined)?.['phase']
        if (phase !== undefined && phase !== 'complete') {
          throw { code: 'GOAL_ALREADY_EXISTS', message: '已有进行中的目标' }
        }
        const objective = String(args?.['objective'] ?? '').trim()
        if (objective === '') throw { code: 'GOAL_INVALID_OBJECTIVE', message: '目标内容不能为空' }
        const maxGoalRounds = Number(args?.['maxGoalRounds'] ?? 256)
        if (!Number.isInteger(maxGoalRounds) || maxGoalRounds <= 0) {
          throw { code: 'GOAL_INVALID_MAX_ROUNDS', message: 'maxGoalRounds 必须是正整数' }
        }
        goalSeq += 1
        goalState = {
          goal: { id: `goal-${goalSeq}`, revision: 1, objective, phase: 'active', maxGoalRounds },
          roundsStarted: 0, createdAt: goalSeq, updatedAt: goalSeq,
        }
        return structuredClone(goalState) as T
      }
      if (command === 'goal_edit' || command === 'goal_pause' || command === 'goal_resume' || command === 'goal_clear') {
        const goal = goalState?.['goal'] as Record<string, unknown> | undefined
        if (goal === undefined) throw { code: 'GOAL_NOT_FOUND', message: '当前没有目标' }
        if (args?.['goalId'] !== goal['id'] || args?.['revision'] !== goal['revision']) {
          throw { code: 'GOAL_STALE_REVISION', message: '目标已被更新，请重读后再试' }
        }
        if (command === 'goal_clear') {
          const tombstone = { id: goal['id'], revision: Number(goal['revision']) + 1 }
          goalState = null
          return tombstone as T
        }
        const next: Record<string, unknown> = { ...goal, revision: Number(goal['revision']) + 1 }
        if (command === 'goal_edit') {
          if (args?.['objective'] === undefined && args?.['maxGoalRounds'] === undefined) {
            throw { code: 'GOAL_INVALID_EDIT', message: '目标编辑至少需要 objective 或 maxGoalRounds' }
          }
          if (args?.['objective'] !== undefined) {
            const objective = String(args['objective']).trim()
            if (objective === '') throw { code: 'GOAL_INVALID_OBJECTIVE', message: '目标内容不能为空' }
            next['objective'] = objective
          }
          if (args?.['maxGoalRounds'] !== undefined) next['maxGoalRounds'] = args['maxGoalRounds']
        } else if (command === 'goal_pause') {
          if (goal['phase'] !== 'active') throw { code: 'GOAL_INVALID_TRANSITION', message: '只有 active 目标可以暂停' }
          next['phase'] = 'paused'
        } else {
          if (goal['phase'] !== 'paused' && goal['phase'] !== 'blocked' && goal['phase'] !== 'active') {
            throw { code: 'GOAL_INVALID_TRANSITION', message: '该阶段不能恢复' }
          }
          next['phase'] = 'active'
        }
        goalState = { ...goalState, goal: next }
        return structuredClone(goalState) as T
      }
      // SH-089：后台作业注册表的宿主替身。缺省空表 / 未找到，避免误把空壳当成功。
      if (command === 'job_list' || command === 'job_read' || command === 'job_kill') {
        if (jobHandler !== undefined) return jobHandler(command, args) as T
        if (command === 'job_list') return [] as T
        throw { code: 'job/not-found', message: `没有作业 ${String(args?.['jobId'])}` }
      }
      if (command === 'save_settings') {
        if (saveFailure !== undefined) throw saveFailure
        // 键名必须是 camelCase：宿主是裸 `#[tauri::command] fn save_settings(base_url, …)`
        // （lib.rs 无 rename_all），Tauri 默认把形参映射成 camelCase。这里读 snake_case
        // 就等于替宿主"宽容"了一次真机上的拒单（D-08 的挡位全失效）。
        state['baseUrl'] = args?.['baseUrl']
        state['model'] = args?.['model']
        state['workspaceRoot'] = args?.['workspaceRoot']
        if (args?.['sandboxMode'] !== undefined) state['sandboxMode'] = args['sandboxMode']
        return undefined as T
      }
      if (command === 'create_session') return 'agent-new' as T
      if (command === 'send_task') return (args?.agentId ?? 'agent-new') as T
      if (command === 'load_session') return archive as T
      // SH-116 批 1：附件读回替身；缺省给一个小的文本文件。
      if (command === 'read_attachment') {
        if (attachmentRead !== undefined) return attachmentRead(args) as T
        return { name: 'notes.txt', mediaType: 'text/plain', bytes: 5, data: btoa('hello'), truncated: false } as T
      }
      // SH-113：宿主 FTS5 派生索引的替身；缺省给一个诚实的空页，不伪造命中。
      if (command === 'search_sessions') {
        if (searchHandler === undefined) return { items: [], hasMore: false } as T
        return searchHandler(args) as T
      }
      if (command === 'list_sessions') {
        return [{ agentId: 'agent-new', updatedAt: 1, title: '首条任务', workspaceRoot: 'W' }] as T
      }
      if (command === 'fork_session') return 'agent-fork' as T
      if (command === 'rename_session') {
        // 模拟宿主语义：去掉首尾空白后为空 = 清除（回 null），否则回生效标题
        if (renameFailure !== undefined) throw renameFailure
        const title = String(args?.title ?? '').trim()
        return (title === '' ? null : title) as T
      }
      // SH-100 的宿主替身：归档/置顶/顺序按真实宿主语义模拟（含集合互斥与排序补位）
      if (command === 'get_workspace_flags') {
        return { archivedSessionIds: [...workspaceFlags.archived], pinnedSessionIds: [...workspaceFlags.pinned], sessionOrder: [...workspaceFlags.order] } as T
      }
      if (command === 'archive_session') {
        const id = String(args?.sessionId ?? '')
        // 运行中的会话：不带 stopActivity 拒绝（session-active），带了先取消再归档
        if (id === 'agent-running' && args?.stopActivity !== true) throw { code: 'session-active', message: `会话 ${id} 正在运行：先停止任务，或携带 stopActivity 归档` }
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        if (!workspaceFlags.archived.includes(id)) workspaceFlags.archived.push(id)
        workspaceFlags.pinned = workspaceFlags.pinned.filter(pinned => pinned !== id)
        return { archivedSessionIds: [...workspaceFlags.archived] } as T
      }
      if (command === 'unarchive_session') {
        const id = String(args?.sessionId ?? '')
        workspaceFlags.archived = workspaceFlags.archived.filter(item => item !== id)
        return { archivedSessionIds: [...workspaceFlags.archived] } as T
      }
      if (command === 'pin_session') {
        const id = String(args?.sessionId ?? '')
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        if (workspaceFlags.archived.includes(id)) throw { code: 'pin-archived', message: `会话 ${id} 已归档：先取消归档才能置顶` }
        if (!workspaceFlags.pinned.includes(id)) workspaceFlags.pinned.unshift(id)
        return { pinnedSessionIds: [...workspaceFlags.pinned] } as T
      }
      if (command === 'unpin_session') {
        const id = String(args?.sessionId ?? '')
        if (id === 'agent-ghost') throw { code: 'not-found', message: `会话档案不存在：${id}` }
        workspaceFlags.pinned = workspaceFlags.pinned.filter(item => item !== id)
        return { pinnedSessionIds: [...workspaceFlags.pinned] } as T
      }
      if (command === 'insert_session_before') {
        const id = String(args?.sessionId ?? '')
        const before = args?.beforeSessionId as string | undefined
        if (id === 'agent-ghost' || before === 'agent-ghost') throw { code: 'not-found', message: '会话档案不存在' }
        if (before !== undefined && !workspaceFlags.order.includes(before)) throw { code: 'move-invalid', message: `锚点会话 ${before} 不在排序账户里` }
        workspaceFlags.order = workspaceFlags.order.filter(item => item !== id)
        if (before === undefined) workspaceFlags.order.push(id)
        else workspaceFlags.order.splice(workspaceFlags.order.indexOf(before), 0, id)
        return { sessionOrder: [...workspaceFlags.order] } as T
      }
      // SH-099 对齐上游（2026-11-07）的宿主替身：默认项的 title/hidden 落
      // [workspace]，额外项落 [[workspaces]] 行——与 lib.rs 的 config 落点同构。
      if (command === 'rename_workspace') {
        const id = String(args?.workspaceId ?? '')
        const title = String(args?.title ?? '').trim()
        if (title === '') throw { code: 'bad-request', message: '工作区名不能为空' }
        const view = (workspaceId: string, path: string, workspaceTitle: string): Record<string, unknown> => ({
          workspaceId, path, title: workspaceTitle, sessionIds: [], createdAt: '', updatedAt: '',
        })
        if (id === 'default-workspace') {
          state['workspaceTitle'] = title
          const stored = state['workspaceTitle']
          return { workspace: view(id, String(state['workspaceRoot'] ?? ''), stored !== undefined && String(stored) !== '' ? String(stored) : 'default-workspace') } as T
        }
        const rows = (state['workspaces'] as { id: string; path: string; title: string }[] | undefined) ?? []
        const row = rows.find(candidate => candidate.id === id)
        if (row === undefined) throw { code: 'not-found', message: `工作区不存在：${id}` }
        row.title = title
        return { workspace: view(id, row.path, title) } as T
      }
      if (command === 'delete_workspace') {
        const id = String(args?.workspaceId ?? '')
        if (id === 'default-workspace') {
          state['workspaceHidden'] = true
          state['workspaceTitle'] = ''
          return { removed: id } as T
        }
        const rows = (state['workspaces'] as { id: string }[] | undefined) ?? []
        if (!rows.some(candidate => candidate.id === id)) throw { code: 'not-found', message: `工作区不存在：${id}` }
        state['workspaces'] = rows.filter(candidate => candidate.id !== id)
        return { removed: id } as T
      }
      if (command === 'create_workspace') {
        const path = String(args?.path ?? '')
        const rows = (state['workspaces'] as { id: string; path: string; title: string }[] | undefined) ?? []
        const view = (workspaceId: string, wsPath: string, title: string): Record<string, unknown> => ({
          workspaceId, path: wsPath, title, sessionIds: [], createdAt: '', updatedAt: '',
        })
        // 与**可见**默认项同路径 = 幂等；hidden 的默认项不复活，走全新注册拿新 id
        // （SH-110：上游 removedIds 不变式要求宿主 id 永不复用）
        if (state['workspaceHidden'] !== true && path === String(state['workspaceRoot'] ?? '')) {
          const stored = state['workspaceTitle']
          return {
            created: false,
            workspace: view('default-workspace', path, stored !== undefined && String(stored) !== '' ? String(stored) : 'default-workspace'),
          } as T
        }
        const existing = rows.find(candidate => candidate.path === path)
        if (existing !== undefined) return { created: false, workspace: view(existing.id, existing.path, existing.title) } as T
        const id = `ws-${rows.length + 1}`
        // get_settings 的 workspaces 行是 {id,path,title}（config 形状），不是 wire 视图
        // 形状——替身写错键名会让 face.workspaces() 读出 id:undefined，baseline 静默退化
        state['workspaces'] = [...rows, { id, path, title: '新目录' }]
        return { created: true, workspace: view(id, path, '新目录') } as T
      }
      // SH-111：顺序落 [workspace].order，回值 = 完整注册表顺序（上游 insertBefore 同构）
      if (command === 'insert_workspace_before') {
        const id = String(args?.workspaceId ?? '')
        const before = args?.['beforeWorkspaceId'] === undefined ? undefined : String(args?.['beforeWorkspaceId'])
        const registry = [
          ...(state['workspaceHidden'] === true ? [] : ['default-workspace']),
          ...((state['workspaces'] as { id: string }[] | undefined) ?? []).map(row => row.id),
        ]
        if (!registry.includes(id)) throw { code: 'not-found', message: `工作区不存在：${id}` }
        if (before !== undefined && !registry.includes(before)) throw { code: 'not-found', message: `工作区不存在：${before}` }
        const stored = (state['workspaceOrder'] as string[] | undefined) ?? []
        const current = [
          ...stored.filter(candidate => registry.includes(candidate)),
          ...registry.filter(candidate => !stored.includes(candidate)),
        ]
        if (before === id) return { workspaceIds: current } as T
        const without = current.filter(candidate => candidate !== id)
        const at = before === undefined ? without.length : without.indexOf(before)
        const next = [...without.slice(0, at), id, ...without.slice(at)]
        state['workspaceOrder'] = next
        return { workspaceIds: next } as T
      }
      return undefined as T
    },
    listenEvents(handler) {
      handlers.add(handler)
      return () => handlers.delete(handler)
    },
    emit(sessionId, msg, agentId = 'root') {
      counter += 1
      const event: AgentEvent = {
        event_id: `01J00000000000000000000${String(counter).padStart(2, '0')}`,
        session_id: sessionId,
        agent_id: agentId,
        parent_id: agentId === 'root' ? null : 'root',
        ts_ms: 1_700_000_000_000,
        msg,
      }
      for (const handler of handlers) handler(event)
    },
  }
}

/** 取快照 records 里的 journal 事件（跳过 assistant-stream 帧）。 */
function eventsOf(snapshot: Record<string, unknown>): Record<string, unknown>[] {
  return (snapshot['records'] as { event: Record<string, unknown> }[]).map(record => record.event)
}

/** 会话面在测试里的窄形状：只声明被测方法，避免每处都写类型断言。 */
interface SessionFace {
  prompt(request: Record<string, unknown>): Promise<Record<string, unknown>>
  follow(request: Record<string, unknown>): AsyncGenerator<Record<string, unknown>>
  page(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  list(): Promise<Record<string, unknown>>
  create(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  cancel(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  fork(request: Record<string, unknown>): Promise<Record<string, unknown>>
  modelCatalog(): Promise<Record<string, unknown>>
  selectModel(request: Record<string, unknown>): Promise<Record<string, unknown>>
  rename(request?: Record<string, unknown>): Promise<Record<string, unknown>>
  attachment(): Promise<never>
  search(request?: Record<string, unknown>): Promise<Record<string, unknown>>
}

interface RemoteFace {
  readonly session: SessionFace
  readonly workspaceFiles: Record<string, () => Promise<unknown>>
  $on(event: string, listener: (this: unknown, ...args: never[]) => unknown): () => void
}

function faceOf(remote: unknown): RemoteFace {
  return remote as RemoteFace
}

describe('SH-116 批 1：附件 prompt 拼装', () => {
  it('文本文件经 read_attachment 内联为 attachment 块，正文与用户文本一起进任务', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const result = await session.prompt({
      sessionId: 'agent-new',
      requestId: 'rpc-attach',
      content: [{ type: 'file', receiptId: 'r1' }, { type: 'text', text: '总结这个文件' }],
    })
    expect(result['ok']).toBe(true)
    const task = String(bridge.calls.find(call => call.command === 'send_task')?.args?.['task'] ?? '')
    expect(task).toContain('**附件 `notes.txt`**（text/plain，5 字节）')
    expect(task).toContain('hello')
    expect(task).toContain('总结这个文件')
    dispose()
  })

  it('二进制附件只给占位，不把字节塞进提示词', async () => {
    const bridge = scriptedBridge(undefined, [], undefined, undefined, undefined, undefined, undefined, undefined, () => ({
      name: 'doc.pdf', mediaType: 'application/pdf', bytes: 99, data: btoa('%PDF'), truncated: false,
    }))
    const { remote, dispose } = createAdapter({ bridge })
    const result = await faceOf(remote).session.prompt({
      sessionId: 'agent-new',
      content: [{ type: 'file', receiptId: 'r2' }],
    })
    expect(result['ok']).toBe(true)
    const task = String(bridge.calls.find(call => call.command === 'send_task')?.args?.['task'] ?? '')
    expect(task).toContain('**附件 `doc.pdf`**（application/pdf，99 字节）（未内联字节）')
    expect(task).not.toContain('%PDF')
    dispose()
  })

  it('图片附件：journal 带 image 块、任务带隐藏标记，session.attachment 可读回', async () => {
    const bridge = scriptedBridge(undefined, [], undefined, undefined, undefined, undefined, undefined, undefined, () => ({
      name: 'shot.png', mediaType: 'image/png', bytes: 4, data: btoa('png!'), truncated: false, width: 2, height: 3,
    }))
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const result = await session.prompt({
      sessionId: 'agent-new',
      content: [{ type: 'file', receiptId: '01ARZ3NDEKTSV4RRFFQ69G5FAV' }, { type: 'text', text: '看图' }],
    })
    expect(result['ok']).toBe(true)
    const task = String(bridge.calls.find(call => call.command === 'send_task')?.args?.['task'] ?? '')
    expect(task).toContain('<!-- shaniu-image:')
    expect(task).toContain('看图')

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const user = eventsOf(snapshot).find(event => event['type'] === 'user/message')
    const content = (user?.['data'] as { content: Record<string, unknown>[] }).content
    expect(content[0]).toMatchObject({
      type: 'image',
      attachment: { attachmentId: '01ARZ3NDEKTSV4RRFFQ69G5FAV', mediaType: 'image/png', width: 2, height: 3 },
    })
    expect(content[1]).toEqual({ type: 'text', text: '看图' })
    await iterator.return?.(undefined)

    const read = await (session as unknown as {
      attachment: (request: { sessionId: string; attachment: Record<string, unknown> }) => Promise<Record<string, unknown>>
    }).attachment({
      sessionId: 'agent-new',
      attachment: { attachmentId: '01ARZ3NDEKTSV4RRFFQ69G5FAV', mediaType: 'image/png', bytes: 4, width: 2, height: 3 },
    })
    expect(read['ok']).toBe(true)
    expect((read['value'] as { data: string }).data).toBe(btoa('png!'))
    dispose()
  })

  it('档案回放：用户文本里的图片标记重建为 image 内容块，标题不带标记', async () => {
    const marker = `<!-- shaniu-image:${encodeURIComponent(JSON.stringify({
      attachmentId: '01ARZ3NDEKTSV4RRFFQ69G5FAV', mediaType: 'image/png', bytes: 4, width: 2, height: 3, name: 'shot.png',
    }))} -->`
    const archive = [
      { kind: 'message', role: 'user', text: `${marker}\n看图` },
      { kind: 'message', role: 'assistant', text: '看到了' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as {
      records: { event: Record<string, unknown> }[]
      projections: { values: { title: string | null } }
    }
    const user = eventsOf(snapshot as unknown as Record<string, unknown>).find(event => event['type'] === 'user/message')
    const content = (user?.['data'] as { content: Record<string, unknown>[] }).content
    expect(content[0]).toMatchObject({ type: 'image', attachment: { attachmentId: '01ARZ3NDEKTSV4RRFFQ69G5FAV' } })
    expect(content[1]).toEqual({ type: 'text', text: '看图' })
    expect(snapshot.projections.values.title).not.toContain('shaniu-image')
    await iterator.return?.(undefined)
    dispose()
  })

  it('缺回执/读回失败显式失败，绝不静默丢附件', async () => {
    const noReceipt = scriptedBridge()
    const noReceiptAdapter = createAdapter({ bridge: noReceipt })
    const first = await faceOf(noReceiptAdapter.remote).session.prompt({
      sessionId: 'agent-new',
      content: [{ type: 'file' }],
    })
    expect(first['ok']).toBe(false)
    expect((first['error'] as Error).message).toContain('回执')

    const broken = scriptedBridge(undefined, [], undefined, undefined, undefined, undefined, undefined, undefined, () => {
      throw new Error('附件不存在')
    })
    const brokenAdapter = createAdapter({ bridge: broken })
    const second = await faceOf(brokenAdapter.remote).session.prompt({
      sessionId: 'agent-new',
      content: [{ type: 'file', receiptId: 'gone' }],
    })
    expect(second['ok']).toBe(false)
    expect((second['error'] as Error).message).toContain('读取附件失败')
    expect(broken.calls.some(call => call.command === 'send_task')).toBe(false)
  })
})

const ENVELOPE_KEYS = ['type', 'seq', 'time', 'data', 'ignorable', 'surfaceOp', 'sourceEventSeqs']
const SURFACE_TYPES = ['user/message', 'assistant/message', 'tool/result']

describe('适配层把 EventMsg 流映射成 DSH 会话 journal', () => {
  it('一次纯文本回合：事件逐条 +1、surface 规则与 wire 校验全过', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '你好' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '你' })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '好' })
    bridge.emit('agent-new', { type: 'token_usage', turn: 1, input_tokens: 10, cached_input_tokens: 2, output_tokens: 4 })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)

    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'assistant/message', 'step/end', 'turn/end',
    ])
    journal.forEach((event, index) => {
      expect(event['seq']).toBe(index)
      expect(Object.keys(event).every(key => ENVELOPE_KEYS.includes(key))).toBe(true)
      expect('surfaceOp' in event).toBe(SURFACE_TYPES.includes(String(event['type'])))
      // 这道断言就是客户端运行时会做的那道闸
      expect(() => assertSessionWireEvent(event)).not.toThrow()
    })
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as Record<string, unknown>
    expect((assistant['message'] as { content: unknown[] }).content).toEqual([{ type: 'text', text: '你好' }])
    // inputTokens 是**未缓存**输入（10 − 2 = 8）：上游 TokenUsage 语义里它是 uncached
    // 桶（usage-projection.ts 的 bucketsFrom 直接拿它当 uncachedInputTokens），
    // total = uncached + output + read。旧断言把全量 prompt 当 input，缓存算了两遍。
    expect(assistant['usage']).toEqual({ inputTokens: 8, outputTokens: 4, cacheReadTokens: 2, cacheWriteTokens: 0, totalTokens: 14 })
    expect((journal.find(event => event['type'] === 'turn/end')?.['data'] as { reason: unknown }).reason)
      .toEqual({ kind: 'completed' })

    await iterator.return?.(undefined)
    dispose()
  })

  it('助手流帧：revision 从快照基线起逐帧 +1，end 引用真实事件 seq', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '想一下' }] })
    // 客户端先打开 follow、再产生活跃流帧：snapshot 的 revision 是基线。
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect((snapshot['assistantStream'] as { revision: number }).revision).toBe(0)
    expect(eventsOf(snapshot).map(event => event['type']))
      .toEqual(['turn/start', 'step/start', 'user/message'])

    bridge.emit('agent-new', { type: 'reasoning_delta', delta: '先' })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '答案' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    // 4 帧助手流 + 1 条 assistant/message 事件帧
    const frames: Record<string, unknown>[] = []
    for (let i = 0; i < 5; i += 1) frames.push((await iterator.next()).value as Record<string, unknown>)

    const stream = frames
      .filter(frame => frame['type'] === 'assistant-stream')
      .map(frame => frame['frame'] as Record<string, unknown>)
    expect(stream.map(frame => frame['revision'])).toEqual([1, 2, 3, 4])
    expect(stream[0]?.['type']).toBe('start')
    expect(stream.at(-1)?.['type']).toBe('end')
    const outcome = stream.at(-1)?.['outcome'] as { kind: string; seq: number }
    expect(outcome.kind).toBe('committed')

    // committed 必须指向真实存在的 assistant/message
    const page = await session.page({ sessionId: 'agent-new' })
    const journal = (page['value'] as { records: { event: Record<string, unknown> }[] }).records.map(record => record.event)
    expect(journal[outcome.seq]?.['type']).toBe('assistant/message')
    expect((journal[outcome.seq]?.['data'] as { message: { content: unknown[] } }).message.content)
      .toEqual([{ type: 'reasoning', text: '先' }, { type: 'text', text: '答案' }])

    await iterator.return?.(undefined)
    dispose()
  })

  it('流式中途打开 follow：快照带 activeAttempt 紧凑前缀，后续 revision 续接不丢正文', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-mid', content: [{ type: 'text', text: '中途接入' }] })
    // 先吐两段，再打开 follow（实时态只渲染用户气泡的 B-20 场景）。
    bridge.emit('agent-new', { type: 'reasoning_delta', delta: '先想' })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '答' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const baseline = snapshot['assistantStream'] as {
      revision: number
      activeAttempt?: {
        attemptId: string
        startedAfterSeq: number
        turn: number
        step: number
        nextIndex: number
        stream: { time: number; chunk: Record<string, unknown> }[]
      }
    }
    expect(baseline.revision).toBe(3)
    expect(baseline.activeAttempt).toBeDefined()
    expect(baseline.activeAttempt?.nextIndex).toBe(2)
    expect(baseline.activeAttempt?.stream.map(item => item.chunk)).toEqual([
      { type: 'reasoning-delta', index: 0, text: '先想' },
      { type: 'text-delta', index: 0, text: '答' },
    ])

    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '案' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    const frames: Record<string, unknown>[] = []
    for (let index = 0; index < 4; index += 1) frames.push((await iterator.next()).value as Record<string, unknown>)
    const stream = frames
      .filter(frame => frame['type'] === 'assistant-stream')
      .map(frame => frame['frame'] as Record<string, unknown>)
    // 基线 revision=3（start + 两段）之后必须是 4（chunk）→ 5（end），不能跳号/重号。
    expect(stream.map(frame => frame['revision'])).toEqual([4, 5])
    expect((stream[0]?.['chunk'] as { text: string }).text).toBe('案')
    expect((stream[1]?.['outcome'] as { kind: string }).kind).toBe('committed')

    await iterator.return?.(undefined)
    dispose()
  })

  it('同一 id 的空白复用不得替换已被 follow 捕获的 journal（B-20 回归）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.create({})
    // 视图先打开 follow（空 journal 的实时快照）。
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    await iterator.next()
    // 选工作区/再次新建可能对同一个空白 id 再调一次 create：不得换掉 follow 已捕获的对象。
    await session.create({})
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-reuse', content: [{ type: 'text', text: '继续' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '答' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const next = await Promise.race([
      iterator.next().then(result => result.value as Record<string, unknown>),
      new Promise<never>((_, reject) =>
        setTimeout(() => reject(new Error('follow 仍挂在被替换的 journal 上：后续帧没有到达')), 300)),
    ])
    // 快照之后必须还能收到实时流（旧实现会永久挂起在这个断言之前）。
    expect(['event', 'assistant-stream']).toContain(next['type'])
    await iterator.return?.(undefined)
    dispose()
  })

  it('两次采样迭代各开一次尝试：同一次内 attemptId 恒定，跨 attempt 不同', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-8', content: [{ type: 'text', text: '两步' }] })
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    await iterator.next()

    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '第一段' })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    bridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '第二段' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const attemptIds: string[] = []
    for (let i = 0; i < 13; i += 1) {
      const frame = (await iterator.next()).value as Record<string, unknown>
      if (frame['type'] === 'assistant-stream') attemptIds.push((frame['frame'] as { attemptId: string }).attemptId)
    }
    // 每次尝试 3 帧（start/chunk/end）：attemptId 若随 revision 变化，
    // 客户端会把同一段的每帧当成"又开了一次尝试"，流式气泡就无法定稿。
    expect(attemptIds).toHaveLength(6)
    expect(new Set(attemptIds.slice(0, 3)).size).toBe(1)
    expect(new Set(attemptIds.slice(3, 6)).size).toBe(1)
    expect(attemptIds[0]).not.toBe(attemptIds[3])

    await iterator.return?.(undefined)
    dispose()
  })

  it('会话流可被中断：follow 挂起等帧时 abort 立刻结束，$stream.dispose 不再悬挂', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const loose = remote as unknown as {
      session: { follow(request: unknown, signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> }
      $stream(options: { name?: string; open: (signal: AbortSignal) => AsyncIterable<unknown> }): {
        [Symbol.asyncIterator](): AsyncIterator<Record<string, unknown>>
        dispose(): Promise<void>
      }
    }
    const request = { address: { kind: 'session', sessionId: 'agent-new' }, assistantStream: true }

    // ① follow 拿到快照后挂起等帧：abort 必须让它立刻 done。生成器若永远挂在
    //    「等下一帧」的 await 上，消费者的 return()/dispose() 就永远不落地
    //    （真机表现：关掉会话再打开，旧流悬着、新内容再也进不来）。
    const abort = new AbortController()
    const iterator = loose.session.follow(request, abort.signal)
    await iterator.next()
    const pending = iterator.next()
    abort.abort(new Error('closed'))
    expect((await pending).done).toBe(true)

    // ② $stream：dispose 必须落地，不能挂在一个永不醒来的生成器上
    const stream = loose.$stream({ name: 'probe', open: signal => loose.session.follow(request, signal) })
    const consumer = stream[Symbol.asyncIterator]()
    await consumer.next()
    const outcome = await Promise.race([
      stream.dispose().then(() => 'settled' as const),
      new Promise<'hung'>(resolve => setTimeout(() => resolve('hung'), 1000)),
    ])
    expect(outcome).toBe('settled')
    dispose()
  })

  it('工作区/投影流同样可中断：workspace.follow 与 session.control 的 dispose 都能落地', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const loose = remote as unknown as {
      workspace: { follow(signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> }
      session: { control(signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> }
      $stream(options: { name?: string; open: (signal: AbortSignal) => AsyncIterable<unknown> }): {
        [Symbol.asyncIterator](): AsyncIterator<Record<string, unknown>>
        dispose(): Promise<void>
      }
    }
    // 这两条在客户端是**进程级**流（workspace-controller / session-controller 的 control
    // 各开一条，只随 Context 拆除而 dispose）。拆不动就会挂住 teardown：dev 热更新与
    // 退出时旧生成器一直悬在「等下一帧」，越积越多。
    const streams: [string, (signal: AbortSignal) => AsyncIterable<unknown>][] = [
      ['workspace.follow', signal => loose.workspace.follow(signal)],
      ['session.control', signal => loose.session.control(signal)],
    ]
    for (const [label, open] of streams) {
      const stream = loose.$stream({ name: label, open })
      const consumer = stream[Symbol.asyncIterator]()
      await consumer.next()
      const outcome = await Promise.race([
        stream.dispose().then(() => 'settled' as const),
        new Promise<'hung'>(resolve => setTimeout(() => resolve('hung'), 1000)),
      ])
      expect(outcome, label).toBe('settled')
    }
    dispose()
  })

  it('工具调用：arguments 落原始 JSON 字符串，输出与退出码并进 tool/result', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '跑一下' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: { cmd: 'ls' } })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: 'a.txt' })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stderr', delta: '警告' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const call = journal.find(event => event['type'] === 'tool/call')?.['data'] as Record<string, unknown>
    expect(call['arguments']).toBe('{"cmd":"ls"}')
    expect(call['callId']).toBe('c1')
    expect(call['name']).toBe('unified_exec')

    const result = journal.find(event => event['type'] === 'tool/result')?.['data'] as Record<string, unknown>
    const message = result['message'] as { content: { text: string }[]; isError: boolean; source: Record<string, unknown> }
    expect(message.content[0]?.text).toBe('a.txt\n警告\n[exit code: 0]')
    expect(message.isError).toBe(false)
    expect(message['source']).toEqual({ kind: 'tool', callId: 'c1' })
    expect(result['error']).toBeUndefined()
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    await iterator.return?.(undefined)
    dispose()
  })

  it('审批拒绝与取消都落成可判定的终态，不靠错误文案猜测', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '删掉它' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c2', tool: 'apply_patch', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c2', exit_code: null, error: '用户拒绝', result: 'denied' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'cancelled' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const resultEvent = journal.find(event => event['type'] === 'tool/result')
    const result = resultEvent?.['data'] as { message: { isError: boolean }; error: { name: string; code: string } }
    expect(result.error.name).toBe('Denied')
    expect(result.message.isError).toBe(true)
    // validator 的前提：带 error 时 isError 必须为真，否则 wire 校验直接抛
    expect(() => assertSessionWireEvent(resultEvent!)).not.toThrow()
    expect((journal.find(event => event['type'] === 'turn/end')?.['data'] as { reason: unknown }).reason)
      .toEqual({ kind: 'aborted', reason: { kind: 'user' } })

    await iterator.return?.(undefined)
    dispose()
  })

  it('采样迭代推进 step 而不是新开端回', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '两轮' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    expect(journal.filter(event => event['type'] === 'turn/start')).toHaveLength(1)
    const steps = journal.filter(event => event['type'] === 'step/start')
    expect(steps).toHaveLength(2)
    expect((steps[1]?.['data'] as { step: number }).step).toBe(1)
    expect(journal.filter(event => event['type'] === 'turn/end')).toHaveLength(1)
    // 没有任何 delta 就不落假正文
    expect(journal.filter(event => event['type'] === 'assistant/message')).toHaveLength(0)

    await iterator.return?.(undefined)
    dispose()
  })

  it('中断收口时半截正文落档并打 interrupted 标记', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-9', content: [{ type: 'text', text: '讲讲' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '说到一半' })
    // 宿主确认取消后才发这条：中断路径的终态由后端给出，适配层不自己判
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'cancelled' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as {
      interrupted?: boolean
      message: { content: unknown[] }
    }
    expect(assistant.interrupted).toBe(true)
    expect(assistant.message.content).toEqual([{ type: 'text', text: '说到一半' }])
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    await iterator.return?.(undefined)
    dispose()
  })

  it('没观察过的会话交空快照，而不是挂着不返回', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-unknown' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect(snapshot['type']).toBe('snapshot')
    expect(snapshot['cursor']).toBe(-1)
    expect(snapshot['records']).toEqual([])
    expect(snapshot['assistantStream']).toEqual({ revision: 0 })
    // 空态之后不该再有帧：下一次 next 必须干净地结束而不是永久挂起
    expect((await iterator.next()).done).toBe(true)
    dispose()
  })
})

describe('适配层的命令面映射', () => {
  it('create 走宿主建档、prompt 带上会话身份', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    expect(await session.create({ cwd: 'W' })).toEqual({ ok: true, value: { sessionId: 'agent-new' } })
    expect(bridge.calls.map(call => call.command)).toContain('create_session')

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-2', content: [{ type: 'text', text: '继续' }] })
    expect(bridge.calls.find(call => call.command === 'send_task')?.args).toEqual({ task: '继续', agentId: 'agent-new' })
    dispose()
  })

  it('list 返回带 sessionId 的 wire 形状，running 反映真实在跑的回合', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const idle = (await session.list())['value'] as { items: Record<string, unknown>[] }
    expect(idle.items[0]).toMatchObject({ sessionId: 'agent-new', cwd: 'W', running: false, formatStatus: 'current' })

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-3', content: [{ type: 'text', text: '在跑' }] })
    const busy = (await session.list())['value'] as { items: { running: boolean }[] }
    expect(busy.items[0]?.running).toBe(true)

    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    const done = (await session.list())['value'] as { items: { running: boolean }[] }
    expect(done.items[0]?.running).toBe(false)
    dispose()
  })

  it('cancel 只转发给宿主，不提前造终态', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-4', content: [{ type: 'text', text: '停' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '半截' })
    await session.cancel({ sessionId: 'agent-new' })
    expect(bridge.calls.some(call => call.command === 'cancel_task')).toBe(true)

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    // 没有 turn_completed 就不该有 turn/end，也不该提前定稿半截正文
    expect(journal.some(event => event['type'] === 'turn/end')).toBe(false)
    expect(journal.some(event => event['type'] === 'assistant/message')).toBe(false)
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    await iterator.return?.(undefined)
    dispose()
  })

  it('rename 写回宿主并同步本地标题缓存，清空后投影回落 null 由客户端派生', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-5', content: [{ type: 'text', text: '第一条消息' }] })

    const renamed = await session.rename({ sessionId: 'agent-new', title: '  我的会话  ' })
    // 宿主回的是去掉首尾空白后的生效标题，适配层原样上交（不替宿主做归一化）
    expect(renamed).toMatchObject({ ok: true, value: { title: '我的会话' } })
    // 请求也原样转发：归一化是宿主（rollout::set_title）的职责，适配层不复制一份规则
    expect(bridge.calls.find(call => call.command === 'rename_session')?.args)
      .toEqual({ agentId: 'agent-new', title: '  我的会话  ' })
    // 本地缓存同步：下一次快照的标题投影用新名字，而不是改名前的旧值
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot.projections.values.title).toBe('我的会话')
    await iterator.return?.(undefined)

    // 空标题 = 清除自定义：投影回落 null（客户端据此派生标题），而不是留下空串
    const cleared = await session.rename({ sessionId: 'agent-new', title: '   ' })
    expect(cleared).toMatchObject({ ok: true, value: { title: '' } })
    const iterator2 = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot2 = (await iterator2.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot2.projections.values.title).toBeNull()
    await iterator2.return?.(undefined)
    dispose()
  })

  it('rename 失败按 wire 契约回 {ok:false}：超长标题映射 session/title-invalid，其余按网关内部错误', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], { code: 'title-invalid', message: '标题最长 120 字，当前 121 字' })
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const invalid = await session.rename({ sessionId: 'agent-new', title: '字'.repeat(121) })
    expect(invalid['ok']).toBe(false)
    const error = invalid['error'] as { name: string; code: string; message: string; details: { sessionId?: string } }
    // DSH 为非法标题声明的正式错误码，不是 rejection——客户端读 result.ok 就能拿到原因
    expect(error.name).toBe('RemoteError')
    expect(error.code).toBe('session/title-invalid')
    expect(error.message).toContain('120')
    expect(error.details.sessionId).toBe('agent-new')
    dispose()

    const bridge2 = scriptedBridge(HOST_SETTINGS, [], { code: 'not-found', message: '会话档案不存在：ghost' })
    const { remote: remote2, dispose: dispose2 } = createAdapter({ bridge: bridge2 })
    const session2 = faceOf(remote2).session
    const missing = await session2.rename({ sessionId: 'ghost', title: '名字' })
    expect(missing['ok']).toBe(false)
    expect((missing['error'] as { code: string }).code).toBe('gateway/internal')
    expect((missing['error'] as { message: string }).message).toContain('不存在')
    dispose2()
  })

  it('page 交出已提交事件，throughSeq 按内层信封的 seq 截断', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-10', content: [{ type: 'text', text: '分页' }] })
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '正文' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const whole = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[]; cursor: number }
    expect(whole.records.map(record => record.event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'assistant/message', 'step/end', 'turn/end',
    ])
    expect(whole.cursor).toBe(5)
    const windowed = (await session.page({ sessionId: 'agent-new', throughSeq: 2 }))['value'] as { records: { event: { seq: number } }[] }
    expect(windowed.records.map(record => record.event.seq)).toEqual([0, 1, 2])
    dispose()
  })

  it('按游标分支用档案对账表换算条目数，表外才拒绝', async () => {
    // 一个条目会展开成多条 journal 事件：seq 与档案条目数不是一回事，
    // 直接把 seq 当 keep_items 传给后端就会从历史中间切断（有 call 无 output → 续跑 400）。
    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'message', role: 'assistant', text: '先看', reasoning: '想' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: '内容' },
      { kind: 'message', role: 'user', text: '第二步' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const journal = eventsOf((await iterator.next()).value as Record<string, unknown>)
    // 档案四条 → 事件 13 条：条目 2（那条 tool_call）展开到的最后一个 seq 是 5，
    // 而 6/7 只是 step/end、turn/end 这类边界事件。截断点落在 8（边界事件上）也必须
    // 只保留前 3 个完整条目——按 seq 直接当条数就会切出"有调用没结果"的历史。
    const forked = await session.fork({ sessionId: 'agent-old', throughSeq: 8 })
    expect(forked['ok']).toBe(true)
    const forkCall = bridge.calls.find(call => call.command === 'fork_session')
    expect(forkCall?.args).toEqual({ agentId: 'agent-old', keepItems: 3 })

    // 截断点落在对账表覆盖不到的位置：宁可不接，也不猜一个条目数
    const beyond = await session.fork({ sessionId: 'agent-old', throughSeq: journal.length + 50 })
    expect(beyond['ok']).toBe(false)
    expect(bridge.calls.filter(call => call.command === 'fork_session')).toHaveLength(1)

    // 分叉出来的会话必须立刻能看到拷来的历史。此前 fork 预置了一个空 journal，
    // follow 因此命中"本进程已观察过"直接交空快照——用户点完分叉看到的是一个
    // 历史全空的会话，而档案里条目明明拷过去了。
    const forkedIterator = session.follow({ address: { kind: 'session', sessionId: 'agent-fork' } })
    const forkedEvents = eventsOf((await forkedIterator.next()).value as Record<string, unknown>)
    expect(forkedEvents.map(event => event.type)).toContain('user/message')
    expect(forkedEvents.map(event => event.type)).toContain('assistant/message')
    await forkedIterator.return?.(undefined)

    // 只有实时回合、没从档案重建过的会话同样没有对账表
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-11', content: [{ type: 'text', text: '分支' }] })
    const unaligned = await session.fork({ sessionId: 'agent-new', throughSeq: 1 })
    expect(unaligned['ok']).toBe(false)
    await iterator.return?.(undefined)
    dispose()
  })

  it('没有审批 UI 在听时按拒绝收口，绝不静默放行', async () => {
    const bridge = scriptedBridge()
    const { dispose } = createAdapter({ bridge })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r1', tool: 'unified_exec', args: {}, risk: 'execute' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r1', decision: 'deny' })
    })
    dispose()
  })

  it('审批卡回答 allowed-once 时回 allow', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    // 第二个参数就是 DSH 审批卡会收到的监听器：它回答 allowed-once / rejected。
    faceOf(remote).$on('approval/request', async () => 'allowed-once')
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r2', tool: 'unified_exec', args: {}, risk: 'execute' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r2', decision: 'allow' })
    })
    dispose()
  })

  it('审批请求按 DSH 声明的载荷派发，并把会话作用域绑给监听器', async () => {
    // 真机踩过一次：监听器是普通函数、靠 `this` 认会话（ui-approval 的第一行就是
    // ctx.sessions.scopeOf(this)）。裸调 listener 会让审批卡根本不出现，后端只看到
    // 一个没人裁决的请求，最终按拒绝收口——表现像"用户拒绝了"，实际是接线错了。
    const bridge = scriptedBridge()
    const scope = { sessionId: 'agent-new' }
    const { remote, dispose } = createAdapter({ bridge, approvalScope: () => scope })
    const seen: { self: unknown; request: Record<string, unknown> }[] = []
    faceOf(remote).$on('approval/request', async function (this: unknown, request: Record<string, unknown>) {
      seen.push({ self: this, request })
      return 'allowed-once'
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r9', tool: 'write_file', args: { path: 'a.txt' }, risk: 'write_in_workspace' })
    await vi.waitFor(() => { expect(seen).toHaveLength(1) })
    expect(seen[0]!.self).toBe(scope)
    expect(seen[0]!.request).toMatchObject({ agent: { id: 'agent-new' }, toolName: 'write_file' })
    // args/risk 不在 DSH 的载荷声明里，必须并进 reason，否则卡片只写工具名不说执行什么
    expect(String(seen[0]!.request['reason'])).toContain('write_file')
    expect(String(seen[0]!.request['reason'])).toContain('a.txt')
    expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r9', decision: 'allow' })
    dispose()
  })

  it('审批监听器抛错时按拒绝收口并留下日志，不冒充用户裁决', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const logged = vi.spyOn(console, 'error').mockImplementation(() => {})
    faceOf(remote).$on('approval/request', async () => {
      throw new Error('作用域取不到')
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r10', tool: 'write_file', args: {}, risk: 'write_in_workspace' })
    await vi.waitFor(() => {
      expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r10', decision: 'deny' })
    })
    // 静默按拒绝回执会让一次接线错误长得像一次正常否决
    expect(logged.mock.calls.some(call => String(call[0]).includes('抛错'))).toBe(true)
    logged.mockRestore()
    dispose()
  })

  it('回合的开与收都广播 api-session/status——Stop 按钮只认这个', async () => {
    // 输入卡的 Stop 条件是 `running && !sending`，而 `running` 来自
    // `api-session/status`（session-controller client 的 $on）与 session.list 摘要，
    // 不看 journal 的 turn/end。只发 journal 不发状态，真机表现就是"跑着长任务
    // 却没有停止按钮"——中断通道本身是通的，用户按不到。
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const seen: [string, boolean][] = []
    const face = remote as unknown as {
      $on(event: string, listener: (...args: never[]) => unknown): () => void
      session: { prompt(request: Record<string, unknown>): Promise<unknown> }
    }
    face.$on('api-session/status', (sessionId: string, running: boolean) => {
      seen.push([sessionId, running])
      return Promise.resolve('')
    })
    await face.session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '跑个长的' }] })
    expect(seen).toEqual([['agent-new', true]])
    bridge.emit('agent-new', { type: 'turn_started', turn: 1 })
    // 同一个回合里不该重复报运行中
    expect(seen).toEqual([['agent-new', true]])
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    expect(seen).toEqual([['agent-new', true], ['agent-new', false]])
    dispose()
  })

  it('工具结果检视流：冷读报不可用，结果落定后交真值', async () => {
    // 详情面板的数据本来就在 journal 里，此前只是没入口：落到"未接线"代理时，
    // 点开工具卡只会报命令面未接线。语义照 DSH host：available:false 与
    // "结果本身是 JSON null" 必须分得开。
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const face = remote as unknown as {
      toolResults: { watch(request: Record<string, unknown>, signal?: AbortSignal): AsyncGenerator<Record<string, unknown>> }
    }

    // 本进程没观察过的会话：不猜一份结果出来，直接不可用
    const cold = face.toolResults.watch({ sessionId: 'agent-ghost', seq: 3 })
    expect((await cold.next()).value).toEqual({ available: false })
    await cold.return(undefined)

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-20', content: [{ type: 'text', text: '列目录' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c9', tool: 'unified_exec', args: { cmd: 'ls' } })
    const pending = face.toolResults.watch({ sessionId: 'agent-new', seq: 999 })
    // 结果还没落定时先交不可用，而不是挂住
    expect((await pending.next()).value).toEqual({ available: false })
    bridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c9', channel: 'stdout', delta: 'port.txt\n' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c9', exit_code: 0, error: null })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as Record<string, unknown>
    const resultSeq = (eventsOf(paged).find(event => event.type === 'tool/result') as { seq: number }).seq
    const live = face.toolResults.watch({ sessionId: 'agent-new', seq: resultSeq })
    const first = (await live.next()).value as { available: boolean; value?: Record<string, unknown> }
    // 落定前那一帧可能是 false（取决于订阅与入队的先后），继续读到 available 为止
    const settledFrame = first.available
      ? first
      : (await live.next()).value as { available: boolean; value?: Record<string, unknown> }
    expect(settledFrame.available).toBe(true)
    expect(settledFrame.value).toMatchObject({ toolCallId: 'c9', content: [{ type: 'text', text: 'port.txt\n\n[exit code: 0]' }] })
    await live.return(undefined)
    await pending.return(undefined)
    dispose()
  })

  it('侧栏冷标题走摘要投影提示块；重开会话时标题从宿主回显预热，不从档案重新派生', async () => {
    const archive = [{ kind: 'message', role: 'user', text: '一条很长的用户消息，本地派生规则只取前十二个字' }]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session

    // list 面：有标题的行带 cached 投影提示块，标题用宿主回显（已合并 titles.json）
    const listed = (await session.list())['value'] as { items: Record<string, unknown>[] }
    expect(listed.items[0]).toMatchObject({
      sessionId: 'agent-new',
      projections: { kind: 'cached', asOfSeq: 0, values: { title: '首条任务' } },
    })

    // 重开：宿主回显标题优先于本地派生——档案首条用户消息的前 12 字不是宿主标题，
    // 若标题被本地重派生，这里会拿到"一条很长的用户消息，本地派"而失败
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as { projections: { values: { title: string | null } } }
    expect(snapshot.projections.values.title).toBe('首条任务')
    await iterator.return?.(undefined)
    dispose()
  })

  it('未接线的命名空间显式抛错并指向核销清单', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    // SH-092 之后 workspaceFiles 已接真，未接线样本改取 terminal（宿主没有终端面）。
    const terminal = (faceOf(remote) as unknown as Record<string, Record<string, (() => Promise<unknown>) | undefined>>).terminal
    await expect(terminal['run']!()).rejects.toThrow(/未接线/)
    await expect(terminal['run']!()).rejects.toThrow(/SH-040/)
    dispose()
  })

  it('重开旧会话：从档案重建 journal，回合边界与 wire 校验都成立', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '把端口改成 9090' },
      { kind: 'message', role: 'assistant', text: '我先看一眼', reasoning: '要确认文件位置' },
      { kind: 'tool_call', call_id: 'c1', tool: 'unified_exec', arguments: '{"cmd":"ls"}', output: 'port.txt\n[exit code: 0]' },
      { kind: 'message', role: 'assistant', text: '改好了' },
      { kind: 'tool_call', call_id: 'c2', tool: 'apply_patch', arguments: '{}', output: '失败\n[exit code: 1]' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)

    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message',
      'assistant/message', 'tool/call', 'tool/result',
      'step/end', 'step/start',
      'assistant/message', 'tool/call', 'tool/result',
      'step/end', 'turn/end',
    ])
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    expect(snapshot['cursor']).toBe(journal.at(-1)?.['seq'])

    // 档案里的思考原文回到 reasoning 块，不混进正文
    const assistant = journal.find(event => event['type'] === 'assistant/message')?.['data'] as {
      message: { content: { type: string }[] }
    }
    expect(assistant.message.content.map(block => block.type)).toEqual(['reasoning', 'text'])
    // 退出码标记是档案里本来就有的尾标记，重建时不能再追加一次
    const results = journal.filter(event => event['type'] === 'tool/result')
      .map(event => (event['data'] as { message: { content: { text: string }[] } }).message.content[0]?.text)
    expect(results[0]).toBe('port.txt\n[exit code: 0]')
    expect((journal.filter(event => event['type'] === 'tool/result')[1]?.['data'] as { message: { isError: boolean } }).message.isError)
      .toBe(true)

    await iterator.return?.(undefined)
    dispose()
  })

  it('没有观察过的会话档案也为空时交空快照，不挂起', async () => {
    const bridge = scriptedBridge(undefined, [])
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-empty' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    expect(snapshot['cursor']).toBe(-1)
    expect(snapshot['records']).toEqual([])
    expect((await iterator.next()).done).toBe(true)
    dispose()
  })

  it('同一份历史：实时跑出来的事件序列与重开时重建的完全一致', async () => {
    // 这条等价性是 lib.rs 里"展示与回放必须同源"的前端面判据：不一致就意味着用户
    // 重开旧会话看到的分组（回合头、用时、工具卡归属）和刚跑完时不一样。
    const liveBridge = scriptedBridge()
    const live = createAdapter({ bridge: liveBridge })
    const liveSession = faceOf(live.remote).session
    await liveSession.prompt({ sessionId: 'agent-new', requestId: 'rpc-eq', content: [{ type: 'text', text: '第一步' }] })
    liveBridge.emit('agent-new', { type: 'agent_message_delta', delta: '先看' })
    liveBridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'read_file', args: {} })
    liveBridge.emit('agent-new', { type: 'tool_call_output_delta', call_id: 'c1', channel: 'stdout', delta: '内容' })
    liveBridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    liveBridge.emit('agent-new', { type: 'turn_started', turn: 2 })
    liveBridge.emit('agent-new', { type: 'agent_message_delta', delta: '改好了' })
    liveBridge.emit('agent-new', { type: 'turn_completed', turn: 2, reason: 'completed' })

    const liveIterator = liveSession.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const liveTypes = eventsOf((await liveIterator.next()).value as Record<string, unknown>)
      .map(event => event['type'])
    await liveIterator.return?.(undefined)
    live.dispose()

    // 同一轮任务落进档案的形状（FunctionCall + FunctionCallOutput 并成一条 ToolCall）
    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'message', role: 'assistant', text: '先看' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: '内容\n[exit code: 0]' },
      { kind: 'message', role: 'assistant', text: '改好了' },
    ]
    const reopened = createAdapter({ bridge: scriptedBridge(undefined, archive) })
    const replayIterator = faceOf(reopened.remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const replayTypes = eventsOf((await replayIterator.next()).value as Record<string, unknown>)
      .map(event => event['type'])
    await replayIterator.return?.(undefined)
    reopened.dispose()

    expect(replayTypes).toEqual(liveTypes)
  })

  it('模型席位只反映宿主真实配置，选模写回服务商文档并推投影', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const catalog = (await session.modelCatalog())['value'] as {
      groups: { id: string; models: { id: string }[] }[]
      routableProviders: string[]
      failures: { id: string; message: string }[]
      default: Record<string, string>
    }
    // 没配密钥的那家进 failures 并带原因，而不是从目录里静默消失：
    // 用户看得见"为什么不能用"，才会去配，而不是以为自己没添加成功。
    expect(catalog.groups).toEqual([{ id: 'longcat', name: 'LongCat', models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5' }] }])
    expect(catalog.routableProviders).toEqual(['longcat'])
    expect(catalog.failures).toEqual([{ id: 'ghost', name: 'Ghost', message: '未配置密钥（引用名 GHOST_API_KEY）' }])
    expect(catalog.default).toEqual({ provider: 'longcat', model: 'LongCat-2.5' })

    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-12', content: [{ type: 'text', text: '换模型' }] })
    const before = bridge.calls.length
    const chosen = await session.selectModel({ sessionId: 'agent-new', provider: 'longcat', model: 'LongCat-2.5' })
    expect(chosen['ok']).toBe(true)
    const write = bridge.calls.slice(before).find(call => call.command === 'save_providers')
    const saved = write?.args as { providers: Record<string, unknown>[]; activeProvider: string; expectedRevision: number }
    // 席位切换写的是**整份**文档：没被选中的那家也必须原样在场，否则切一次模型
    // 就把别的服务商删了。
    expect(saved.providers.map(row => row.id)).toEqual(['longcat', 'ghost'])
    expect(saved.activeProvider).toBe('longcat')
    expect(saved.expectedRevision).toBe(4)
    expect(saved.providers[0]).toMatchObject({
      id: 'longcat',
      base_url: 'https://lc/v1',
      secret_ref: 'LONGCAT_API_KEY',
      model: 'LongCat-2.5',
      // 嵌套结构体按 04 §4 的落盘名送，Tauri 只转顶层参数名
      models: [{ id: 'LongCat-2.5', name: 'LongCat 2.5', context_window: 128_000, max_tokens: 8_192, input: ['text'] }],
    })
    expect(saved.providers[1]).toMatchObject({ id: 'ghost', model: 'g-1', secret_ref: 'GHOST_API_KEY' })
    dispose()
  })

  it('席位选一个清单外的模型就是拒绝，不写出一份后端会拒收的文档', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const result = await faceOf(remote).session.selectModel({ sessionId: 'a', provider: 'longcat', model: '不存在的模型' })
    expect(result['ok']).toBe(false)
    expect((result['error'] as Error).message).toContain('模型清单里没有')
    expect(bridge.calls.some(call => call.command === 'save_providers')).toBe(false)
    dispose()
  })

  it('只有 [model] 回落段的安装：席位显示未配置，不编一个存不回密钥的档位', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, activeProvider: 'openai-responses', providerRevision: 0, hasApiKey: true, providers: [] })
    const { remote, dispose } = createAdapter({ bridge })
    const catalog = (await faceOf(remote).session.modelCatalog())['value'] as { groups: unknown[]; default: Record<string, string> }
    // `[model].provider` 带连字符，不是 DSH 合法的凭据引用名；把它当档位显示，
    // 用户在界面里填的 Key 会存成 OPENAI_RESPONSES_API_KEY，而后端读的是 openai-responses。
    expect(catalog.groups).toEqual([])
    expect(catalog.default).toEqual({ provider: '', model: '' })
    dispose()
  })
})

describe('设置 / 凭据 / 目录三张面经 remote 可达', () => {
  interface FaceProxy {
    settings: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    llm: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    credentials: Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    $on(event: string, listener: (...args: never[]) => unknown): () => void
  }

  const faces = (remote: unknown): FaceProxy => remote as FaceProxy

  it('三个命名空间由适配层直供，不再落到"未接线"代理', async () => {
    const { remote, dispose } = createAdapter({ bridge: scriptedBridge() })
    const face = faces(remote)
    expect((await face.settings['describe']!())['ok']).toBe(true)
    const listed = await face.llm['listConfigurableProviders']!()
    expect((listed['value'] as unknown[]).length).toBeGreaterThan(0)
    const described = await face.credentials['describe']!(['LONGCAT_API_KEY', 'GHOST_API_KEY'])
    expect(described['value']).toMatchObject({
      LONGCAT_API_KEY: { configured: true, source: 'shaniu-dpapi' },
      GHOST_API_KEY: { configured: false },
    })
    dispose()
  })

  it('存密钥后向已注册的监听器广播；退订后不再收到', async () => {
    const { remote, dispose } = createAdapter({ bridge: scriptedBridge() })
    const face = faces(remote)
    const seen: string[] = []
    const off = face.$on('llm/adapters-updated', () => { seen.push('adapters') })
    await face.credentials['set']!('MINIMAX_API_KEY', 'sk-m')
    expect(seen).toEqual(['adapters'])
    off()
    await face.credentials['set']!('MINIMAX_API_KEY', 'sk-m')
    // 退订之后不该再被叫到，否则监听器会越积越多
    expect(seen).toEqual(['adapters'])
    dispose()
  })
})

describe('权限挡位与底部指标的数据面', () => {
  interface PermissionTestFace {
    permissionPresets: { catalog(): Promise<Record<string, unknown>> }
    commands: { execute(sessionId: string, line: string, args?: readonly string[]): Promise<Record<string, unknown>> }
    $on(event: string, listener: (...args: unknown[]) => unknown): () => void
    settings: {
      describe(): Promise<Record<string, unknown>>
      mutate(ns: string, ops: Record<string, unknown>[], expectedRevision?: number): Promise<Record<string, unknown>>
    }
    session: SessionFace & {
      control(): AsyncGenerator<Record<string, unknown>>
      updateQueue(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOf2 = (remote: unknown): PermissionTestFace => remote as PermissionTestFace

  it('挡位目录随宿主沙箱档反推默认，切换写 save_settings 并让快照/基线显示新挡位', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)

    // 目录：静态三挡（无实验档 auto），默认反推自宿主 sandboxMode（workspace_write）
    const catalog = (await face.permissionPresets.catalog())['value'] as {
      options: { value: string; name: string }[]
      defaultOptions: { value: string }[]
      defaultPreset: string
    }
    expect(catalog.options.map(option => option.value)).toEqual(['read-only', 'workspace-write', 'danger-full-access'])
    expect(catalog.defaultOptions.map(option => option.value)).toEqual(['read-only', 'workspace-write', 'danger-full-access'])
    expect(catalog.defaultPreset).toBe('workspace-write')

    // 切挡前的快照基线：挡位投影 = 宿主当前档
    await face.session.create({})
    const before = face.session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snap1 = (await before.next()).value as { projections: { values: Record<string, unknown> } }
    expect(snap1.projections.values['permissions']).toEqual({ currentValue: 'workspace-write' })
    await before.return?.(undefined)

    // 提交：/permission 命令 → save_settings，其余必填参数传宿主现值原样写回。
    // 顶层键是 camelCase（Tauri 形参映射；D-08 真机错误正是拿到了 snake_case）
    const callsBefore = bridge.calls.length
    const result = await face.commands.execute('agent-new', '/permission danger-full-access')
    // SH-115 起结果走 CommandResult 契约（value.result），与上游宿主命令同形状
    expect(result).toMatchObject({ ok: true, value: { result: { kind: 'success', text: 'preset danger-full-access' } } })
    expect(bridge.calls.slice(callsBefore).find(call => call.command === 'save_settings')?.args)
      .toEqual({
        baseUrl: 'https://lc/v1', model: 'LongCat-2.5', workspaceRoot: 'W', sandboxMode: 'danger_full_access',
      })

    // 宿主落盘后 get_settings 回读新值：目录默认与重开快照都反映新挡位
    const catalog2 = (await face.permissionPresets.catalog())['value'] as { defaultPreset: string }
    expect(catalog2.defaultPreset).toBe('danger-full-access')
    const after = face.session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snap2 = (await after.next()).value as { projections: { values: Record<string, unknown> } }
    expect(snap2.projections.values['permissions']).toEqual({ currentValue: 'danger-full-access' })
    await after.return?.(undefined)

    // control 基线同样带挡位（客户端重连时不丢当前值）
    const control = face.session.control()
    const baseline = (await control.next()).value as {
      value: { projections: Record<string, { values: Record<string, unknown> }> }
    }
    expect(baseline.value.projections['agent-new']?.values['permissions'])
      .toEqual({ currentValue: 'danger-full-access' })
    await control.return?.(undefined)
    dispose()
  })

  it('commands.execute：真正未命中的行回 undefined；/permission 的 bare 与未知挡位按上游语义回 CommandResult', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    // matched 判据是"返回值非 undefined"（session.ts）：未命中必须 ok:true + value undefined
    const other = await face.commands.execute('agent-new', '/nonexistent')
    expect(other).toMatchObject({ ok: true })
    expect(other['value']).toBeUndefined()
    // 未知挡位：上游语义是"命中但 handler 报错"（CommandResult error），不是未命中
    const unknownPreset = await face.commands.execute('agent-new', '/permission auto')
    expect(unknownPreset).toMatchObject({
      ok: true,
      value: { result: { kind: 'error', text: expect.stringContaining('unknown preset "auto"') } },
    })
    // bare：回当前挡位与可用清单（上游 permission-presets 同款文案）
    const bare = await face.commands.execute('agent-new', '/permission')
    expect(bare).toMatchObject({
      ok: true,
      value: { result: { kind: 'success', text: expect.stringContaining('current preset') } },
    })
    expect(bridge.calls.some(call => call.command === 'save_settings')).toBe(false)
    dispose()
  })

  it('挡位切换被宿主拒绝时回 CommandResult error（命中命令、草稿保留），目录默认保持旧档', async () => {
    // SH-115 起对齐上游：handler 失败 = CommandResult error（composer 读 result.kind
    // 保留草稿），transport 层的 {ok:false} 留给"调用本身被拒"的场合
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, '沙箱模式取值非法，可用：read_only / workspace_write / danger_full_access')
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    const result = await face.commands.execute('agent-new', '/permission danger-full-access')
    expect(result).toMatchObject({
      ok: true,
      value: { result: { kind: 'error', text: expect.stringContaining('沙箱模式取值非法') } },
    })
    const catalog = (await face.permissionPresets.catalog())['value'] as { defaultPreset: string }
    expect(catalog.defaultPreset).toBe('workspace-write')
    dispose()
  })

  it('设置页写 defaultPreset（settings 面的 permission 域）同样推 permissions 投影', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    await face.session.create({})
    const control = face.session.control()
    const baseline = (await control.next()).value as {
      value: { projections: Record<string, { values: Record<string, unknown> }> }
    }
    expect(baseline.value.projections['agent-new']?.values['permissions']).toEqual({ currentValue: 'workspace-write' })

    // 设置页的挡位行走的就是这条写面：describe 找 permission 域 → mutate 写 defaultPreset
    const described = (await face.settings.describe())['value'] as {
      namespaces: { ns: string; revision: number; value: Record<string, unknown> }[]
    }
    const permission = described.namespaces.find(ns => ns['ns'] === 'permission')!
    expect(permission.value).toEqual({ defaultPreset: 'workspace-write' })
    const result = await face.settings.mutate(
      'permission',
      [{ op: 'set', path: ['defaultPreset'], value: 'read-only' }],
      permission.revision,
    )
    expect(result['ok']).toBe(true)
    expect(bridge.calls.find(call => call.command === 'save_settings')?.args)
      .toMatchObject({ sandboxMode: 'read_only' })

    // 两条写面（/permission 命令与设置行）必须给出同一种确认：一帧 permissions 投影。
    // 少了这一帧，用户在设置页改完档，回到对话页芯片上还是旧档。
    const frame = (await control.next()).value as { changes: { key: string; val: unknown }[] }
    expect(frame.changes).toEqual([{ key: 'permissions', val: { currentValue: 'read-only' } }])
    await control.return?.(undefined)
    dispose()
  })

  it('芯片与设置页写挡位走同一条收尾链：/permission 也让 describe 的 permission 视图前进', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf2(remote)
    const emitted: unknown[][] = []
    face.$on('settings/document-updated', (...args: unknown[]) => { emitted.push(args) })

    const permissionView = async (): Promise<{ value: Record<string, unknown>; revision: number }> => {
      const described = (await face.settings.describe())['value'] as {
        namespaces: { ns: string; value: Record<string, unknown>; revision: number }[]
      }
      const found = described.namespaces.find(entry => entry['ns'] === 'permission')!
      return { value: found.value, revision: found.revision }
    }
    // 设置页先读一次并缓存（真机 E-06 的起点：缓存里是上一档）
    expect(await permissionView()).toEqual({ value: { defaultPreset: 'workspace-write' }, revision: 0 })

    // ① 芯片路径：/permission 命令。收尾必须广播 + 推进修订号——设置页的 describe 缓存
    //    只认这条通知重读，缺了它就是"芯片变了、开设置还是旧档"
    const viaCommand = await face.commands.execute('agent-new', '/permission read-only')
    expect(viaCommand).toMatchObject({ ok: true, value: { result: { kind: 'success', text: 'preset read-only' } } })
    expect(emitted).toContainEqual(['permission', 1])
    expect(await permissionView()).toEqual({ value: { defaultPreset: 'read-only' }, revision: 1 })

    // ② 设置页路径：mutate。两条路径的最终视图与修订号节奏完全一致
    const viaSettings = await face.settings.mutate('permission', [
      { op: 'set', path: ['defaultPreset'], value: 'danger-full-access' },
    ], 1)
    expect(viaSettings['ok']).toBe(true)
    const afterMutate = await permissionView()
    expect(afterMutate).toEqual({ value: { defaultPreset: 'danger-full-access' }, revision: 2 })
    expect(viaSettings['value']).toMatchObject(afterMutate)
    expect(emitted).toEqual([['permission', 1], ['permission', 2]])
    dispose()
  })

  it('底部指标：回合/步/墙钟折叠成 sessionStats，首个采样后用量与占用投影出现', async () => {
    const bridge = scriptedBridge()
    // 受控时钟：墙钟口径的断言（llm/ttft/decode）必须确定性，不赌事件处理耗时
    let clock = 1_000_000
    const { remote, dispose } = createAdapter({ bridge, now: () => clock })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-m1', content: [{ type: 'text', text: '指标' }] })
    clock += 100
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '正文' })
    clock += 200
    bridge.emit('agent-new', { type: 'token_usage', turn: 1, input_tokens: 10, cached_input_tokens: 2, output_tokens: 4 })
    clock += 300
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    // llmMs = step/start → 消息落定（600）；ttftMs = 起点 → 首 delta（100）；
    // decodeMs = 首 delta → 落定（500），decodeTokens 取该次采样的输出桶
    expect(values['sessionStats']).toEqual({
      turns: 1, steps: 1, llmMs: 600, toolMs: 0, ttftMs: 100, ttftSteps: 1, decodeMs: 500, decodeTokens: 4,
    })
    // 用量四桶：uncached = 10 − 2 = 8（缓存只算一次）
    expect(values['tokenUsage']).toEqual({ uncachedInputTokens: 8, outputTokens: 4, cacheReadTokens: 2, cacheWriteTokens: 0 })
    // 占用：分子 = prompt 侧全量（10），分母来自生效模型的 contextWindow
    expect(values['contextPressure']).toEqual({ pressureTokens: 10, projectedTokens: 10, contextWindow: 128_000 })
    await iterator.return?.(undefined)
    dispose()
  })

  it('底部指标：工具墙钟按 callId 配对结算，没等到结果的调用不计时长', async () => {
    const bridge = scriptedBridge()
    let clock = 2_000_000
    const { remote, dispose } = createAdapter({ bridge, now: () => clock })
    const session = faceOf(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-m2', content: [{ type: 'text', text: '跑工具' }] })
    clock += 100
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c1', tool: 'unified_exec', args: {} })
    clock += 700
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'c1', exit_code: 0, error: null, result: 'ok' })
    clock += 100
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'c2', tool: 'read_file', args: {} })
    clock += 100
    // c2 没等到 tool_call_end 就收口：漏算时长是诚实缺省，不编一个值
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    expect(values['sessionStats']).toEqual({
      turns: 1, steps: 1, llmMs: 0, toolMs: 700, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
    })
    // 没烧过 token：用量/占用投影不挂键，pill 与仪表按缺席隐藏
    expect(values['tokenUsage']).toBeUndefined()
    expect(values['contextPressure']).toBeUndefined()
    await iterator.return?.(undefined)
    dispose()
  })

  it('重开旧会话：回放折叠出回合/步计数，没有可信时刻与用量就不伪造', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '第一问' },
      { kind: 'message', role: 'assistant', text: '答一' },
      { kind: 'tool_call', call_id: 'c1', tool: 'read_file', arguments: '{}', output: 'ok\n[exit code: 0]' },
      { kind: 'message', role: 'user', text: '第二问' },
      { kind: 'message', role: 'assistant', text: '答二' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOf(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    expect(values['sessionStats']).toEqual({
      turns: 2, steps: 2, llmMs: 0, toolMs: 0, ttftMs: 0, ttftSteps: 0, decodeMs: 0, decodeTokens: 0,
    })
    expect(values['tokenUsage']).toBeUndefined()
    expect(values['contextPressure']).toBeUndefined()
    expect(values['permissions']).toEqual({ currentValue: 'workspace-write' })
    await iterator.return?.(undefined)
    dispose()
  })
})

describe('Plan 芯片与客户端排队', () => {
  interface QueueTestFace {
    commands: {
      execute(sessionId: string, line: string): Promise<Record<string, unknown>>
      list(sessionId: string): Promise<Record<string, unknown>>
    }
    session: SessionFace & {
      updateQueue(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOf3 = (remote: unknown): QueueTestFace => remote as QueueTestFace

  /** 开一次 follow 取投影值（用完即关，避免生成器挂住）。 */
  async function projectionValuesOf(session: SessionFace, sessionId: string): Promise<Record<string, unknown>> {
    const iterator = session.follow({ address: { kind: 'session', sessionId } })
    const values = ((await iterator.next()).value as { projections: { values: Record<string, unknown> } }).projections.values
    await iterator.return?.(undefined)
    return values
  }

  it('Plan 芯片：enter/exit_plan_mode 按执行结果驱动 plan 投影，被拒的退出保持计划态', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-p1', content: [{ type: 'text', text: '做个计划' }] })

    // enter_plan_mode 免审批必执行 → 计划态开
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'e1', tool: 'enter_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'e1', exit_code: 0, error: null, result: 'ok' })
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true, pending: false } })

    // exit_plan_mode 被拒（审批卡点了拒绝）→ 保持计划态，与宿主 PlanState 一致
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'x1', tool: 'exit_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'x1', exit_code: null, error: '用户拒绝', result: 'denied' })
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true } })

    // 获批执行 → 退出，投影键缺席（芯片与 plan 占位都按缺席判读）
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'x2', tool: 'exit_plan_mode', args: {} })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'x2', exit_code: 0, error: null, result: 'ok' })
    expect((await projectionValuesOf(session, 'agent-new'))['plan']).toBeUndefined()
    dispose()
  })

  it('重开旧会话：计划态从档案重建——exit 有输出（获批）才算退出', async () => {
    const plannedAndExited = [
      { kind: 'message', role: 'user', text: '规划一下' },
      { kind: 'tool_call', call_id: 'e1', tool: 'enter_plan_mode', arguments: '{}', output: '已进入计划模式' },
      { kind: 'tool_call', call_id: 'x1', tool: 'exit_plan_mode', arguments: '{}', output: '计划已批准' },
    ]
    const exited = createAdapter({ bridge: scriptedBridge(undefined, plannedAndExited) })
    const values1 = await projectionValuesOf(faceOf(exited.remote).session, 'agent-old')
    expect(values1['plan']).toBeUndefined()
    exited.dispose()

    const stillPlanning = [
      { kind: 'message', role: 'user', text: '规划一下' },
      { kind: 'tool_call', call_id: 'e1', tool: 'enter_plan_mode', arguments: '{}', output: '已进入计划模式' },
    ]
    const planning = createAdapter({ bridge: scriptedBridge(undefined, stillPlanning) })
    const values2 = await projectionValuesOf(faceOf(planning.remote).session, 'agent-old')
    expect(values2['plan']).toEqual({ active: true, pending: false })
    planning.dispose()
  })

  it('/plan 真实进出计划态（SH-115：宿主 set_plan_mode 面），run/done 事件随行', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const session = face.session

    // bare /plan：进入 + 投影随行
    const activate = await face.commands.execute('agent-new', '/plan')
    expect(activate).toMatchObject({ ok: true, value: { result: { kind: 'success', text: 'Plan mode on. Use /plan off to leave.' } } })
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true, pending: false } })
    // 宿主替身确实被置位（进程级状态，与工具路径同源）
    expect(bridge.calls.some(call => call.command === 'set_plan_mode' && call.args?.['active'] === true)).toBe(true)

    // /plan off：真实退出（替换 SH-106 期间的"指引"方案，对齐上游 PlanChip 退出路径）
    const off = await face.commands.execute('agent-new', '/plan off')
    expect(off).toMatchObject({ ok: true, value: { result: { kind: 'success', text: 'Plan mode off.' } } })
    expect((await projectionValuesOf(session, 'agent-new'))['plan']).toBeUndefined()

    // 生命周期事件成对出现在活动 journal（仅活态；落档缺口已在卡上声明）
    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const events = eventsOf((await iterator.next()).value as Record<string, unknown>)
    await iterator.return?.(undefined)
    const runs = events.filter(event => event['type'] === 'command/run') as { data: Record<string, unknown> }[]
    const dones = events.filter(event => event['type'] === 'command/done') as { data: Record<string, unknown> }[]
    expect(runs.map(event => event.data['name'])).toEqual(['plan', 'plan'])
    expect(dones.map(event => event.data['text'])).toEqual(['Plan mode on. Use /plan off to leave.', 'Plan mode off.'])
    dispose()
  })

  it('/plan <message>：空闲态进入计划态并提交消息；运行中如实拒绝注入', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const session = face.session

    // 空闲态：进入计划态 + 消息走与用户输入相同的提交链（send_task + 回合事件）
    const withMessage = await face.commands.execute('agent-new', '/plan 先调研再动手')
    expect(withMessage).toMatchObject({ ok: true, value: { result: { kind: 'success' } } })
    expect(bridge.calls.some(call => call.command === 'send_task' && call.args?.['task'] === '先调研再动手')).toBe(true)
    expect(await projectionValuesOf(session, 'agent-new')).toMatchObject({ plan: { active: true } })

    // 运行中：没有注入面（SH-053），报错让草稿留在输入框，不伪造受理
    const busy = await face.commands.execute('agent-new', '/plan 再补一条')
    expect(busy).toMatchObject({ ok: true, value: { result: { kind: 'error', text: expect.stringContaining('SH-053') } } })
    dispose()
  })

  it('/compact：成功推 run + 摘要 user 消息 + done；无可压历史与失败各回各的文案', async () => {
    const bridge = scriptedBridge(undefined, undefined, undefined, undefined, undefined, () => ({
      summary: '五段式交接摘要',
      shadowedItems: 12,
      shadowedTokens: 3456,
    }))
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const session = face.session

    const ok = await face.commands.execute('agent-new', '/compact')
    expect(ok).toMatchObject({
      ok: true,
      value: { result: { kind: 'success', text: 'Compacted 12 history items (~3456 tokens).' } },
    })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const events = eventsOf((await iterator.next()).value as Record<string, unknown>)
    await iterator.return?.(undefined)
    const types = events.map(event => event['type'])
    expect(types).toContain('command/run')
    expect(types).toContain('command/done')
    const summary = events.filter(event => event['type'] === 'user/message').at(-1) as { data: { content: { text: string }[] } }
    expect(summary.data.content[0]!['text']).toBe('五段式交接摘要')

    // 带参数：上游 USAGE 文案原样拒绝
    const usage = await face.commands.execute('agent-new', '/compact 现在')
    expect(usage).toMatchObject({ ok: true, value: { result: { kind: 'error', text: 'Usage: /compact (no arguments)' } } })

    // 宿主失败：错误文本如实回传（含宿主原因），不吞成"没搜到历史"
    const failingBridge = scriptedBridge(undefined, undefined, undefined, undefined, undefined, () => {
      throw '压缩总结流失败: 端点 500'
    })
    const failing = createAdapter({ bridge: failingBridge })
    const fail = await faceOf3(failing.remote).commands.execute('agent-new', '/compact')
    expect(fail).toMatchObject({ ok: true, value: { result: { kind: 'error', text: '压缩总结流失败: 端点 500' } } })
    failing.dispose()
    dispose()
  })

  it('/export：触发 header-actions 注册的下载链；未装配时如实报错', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)

    // 未注册执行者（header-actions 尚未装配）：不伪造"已请求"
    const missing = await face.commands.execute('agent-new', '/export')
    expect(missing).toMatchObject({ ok: true, value: { result: { kind: 'error', text: expect.stringContaining('not available') } } })

    // 注册后：命中执行者并带上会话 id
    const seen: string[] = []
    const unregister = registerExportTrigger((sessionId) => { seen.push(sessionId) })
    const ok = await face.commands.execute('agent-new', '/export')
    expect(ok).toMatchObject({ ok: true, value: { result: { kind: 'success', text: 'Session log download requested.' } } })
    expect(seen).toEqual(['agent-new'])
    unregister()
    dispose()
  })

  it('命令目录：五条宿主命令带第一方 definitionId（菜单行与中文别名据此渲染）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    expect((await face.commands.list('agent-new'))['value']).toEqual([
      { definitionId: '@deepseek-ai/dsh-command-compact', name: 'compact', description: 'Compact older conversation history' },
      { definitionId: '@deepseek-ai/dsh-plan-mode', name: 'plan', description: 'Enter or leave plan mode', input: { hint: '[off|message]', attachments: false } },
      { definitionId: '@deepseek-ai/dsh-command-goal', name: 'goal', description: 'Set or view the goal for a long-running task', input: { hint: '[<objective>|clear|edit <objective>|pause|resume]', attachments: false } },
      { definitionId: '@deepseek-ai/dsh-permission-presets', name: 'permission', description: 'Switch the permission preset (sandbox mode + approval policy)', input: { hint: '<preset>' } },
      { definitionId: '@deepseek-ai/dsh-session-log-export', name: 'export', description: 'Download this Session log as a ZIP archive' },
    ])
    dispose()
  })

  it('/goal：创建/查看/暂停/恢复/编辑/清除，非法语法与重复创建报上游口径错误', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const textOf = (result: Record<string, unknown>) =>
      (result as { value: { result: { text: string } } }).value.result.text
    const kindOf = (result: Record<string, unknown>) =>
      (result as { value: { result: { kind: string } } }).value.result.kind

    // 空目录：bare 命令教用法
    expect(textOf(await face.commands.execute('agent-new', '/goal'))).toContain('No goal is currently set.')
    expect(kindOf(await face.commands.execute('agent-new', '/goal'))).toBe('success')

    // 创建
    const created = await face.commands.execute('agent-new', '/goal 把 SH-088 修绿')
    expect(kindOf(created)).toBe('success')
    expect(textOf(created)).toContain('Goal created')
    expect(textOf(created)).toContain('Objective: 把 SH-088 修绿')
    expect(textOf(created)).toContain('Rounds: 0/256')

    // 已有进行中的目标：拒绝重复创建（不覆盖）
    const duplicate = await face.commands.execute('agent-new', '/goal 再开一个')
    expect(kindOf(duplicate)).toBe('error')
    expect(textOf(duplicate)).toContain('A goal is already active.')

    expect(textOf(await face.commands.execute('agent-new', '/goal pause'))).toContain('Goal paused')
    expect(textOf(await face.commands.execute('agent-new', '/goal resume'))).toContain('Goal resumed')
    expect(textOf(await face.commands.execute('agent-new', '/goal edit 换成新目标'))).toContain('Goal updated')
    expect(textOf(await face.commands.execute('agent-new', '/goal edit 换成新目标'))).toContain('Objective: 换成新目标')

    const bareEdit = await face.commands.execute('agent-new', '/goal edit')
    expect(kindOf(bareEdit)).toBe('error')
    expect(textOf(bareEdit)).toContain('Goal editing requires a replacement objective.')

    expect(textOf(await face.commands.execute('agent-new', '/goal clear'))).toBe('Goal cleared.')
    expect(textOf(await face.commands.execute('agent-new', '/goal clear'))).toBe('No goal to clear.')
    dispose()
  })

  it('remote.goals：create/get/pause/resume/edit/clear 与 activation 边沿、CAS 拒绝', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = remote as unknown as {
      goals: {
        get(sessionId: string): Promise<Record<string, unknown>>
        create(sessionId: string, request: Record<string, unknown>): Promise<Record<string, unknown>>
        edit(sessionId: string, ref: Record<string, unknown>, request: Record<string, unknown>): Promise<Record<string, unknown>>
        pause(sessionId: string, ref: Record<string, unknown>): Promise<Record<string, unknown>>
        resume(sessionId: string, ref: Record<string, unknown>): Promise<Record<string, unknown>>
        clear(sessionId: string, ref: Record<string, unknown>): Promise<Record<string, unknown>>
      }
      $on(event: string, listener: (event: Record<string, unknown>) => void): () => void
    }
    const activations: Record<string, unknown>[] = []
    const off = face.$on('goal/activation-changed', event => { activations.push(event) })

    expect(await face.goals.get('agent-new')).toMatchObject({ ok: true, value: undefined })

    expect(await face.goals.create('agent-new', { objective: '把 SH-088 修绿' })).toMatchObject({
      ok: true, value: { ref: { id: 'goal-1', revision: 1 } },
    })
    expect(activations.at(-1)).toMatchObject({
      sessionId: 'agent-new', goal: { id: 'goal-1', revision: 1, activation: 'armed' },
    })
    expect(await face.goals.get('agent-new')).toMatchObject({
      ok: true,
      value: {
        id: 'goal-1', revision: 1, objective: '把 SH-088 修绿', phase: 'active',
        activation: 'armed', roundsStarted: 0, maxGoalRounds: 256,
      },
    })

    // 旧 revision 被宿主 CAS 拒绝，进程内 activation 不动
    expect(await face.goals.pause('agent-new', { id: 'goal-1', revision: 99 })).toMatchObject({
      ok: false, error: { code: 'GOAL_STALE_REVISION' },
    })
    expect((await face.goals.get('agent-new')) as { value: { activation: string } }).toMatchObject({
      value: { activation: 'armed' },
    })

    expect(await face.goals.pause('agent-new', { id: 'goal-1', revision: 1 })).toMatchObject({
      ok: true, value: { phase: 'paused', revision: 2, activation: 'disarmed' },
    })
    expect(activations.at(-1)).toMatchObject({ sessionId: 'agent-new', goal: { activation: 'disarmed' } })

    expect(await face.goals.resume('agent-new', { id: 'goal-1', revision: 2 })).toMatchObject({
      ok: true, value: { phase: 'active', revision: 3, activation: 'armed' },
    })

    // edit 不产生 activation 边沿（上游语义）
    const beforeEdit = activations.length
    expect(await face.goals.edit('agent-new', { id: 'goal-1', revision: 3 }, { objective: '换一个目标', maxGoalRounds: 8 })).toMatchObject({
      ok: true, value: { objective: '换一个目标', maxGoalRounds: 8, revision: 4, activation: 'armed' },
    })
    expect(activations).toHaveLength(beforeEdit)

    expect(await face.goals.clear('agent-new', { id: 'goal-1', revision: 4 })).toMatchObject({
      ok: true, value: { id: 'goal-1', revision: 5 },
    })
    expect(activations.at(-1)).toEqual({ sessionId: 'agent-new' })
    expect(await face.goals.get('agent-new')).toMatchObject({ ok: true, value: undefined })
    off()
    dispose()
  })

  it('remote.job：list 整表帧、follow opened→output→status、kill 类型化失败', async () => {
    const running = {
      id: 'bash-1', kind: 'bash', label: 'sleep', status: 'running', startedAt: 1,
      output: { total: 3, earliest: 0 },
    }
    const done = {
      id: 'bash-2', kind: 'bash', label: 'echo hi', status: 'completed', startedAt: 2,
      finishedAt: 3, detail: 'exit code: 0', output: { total: 6, earliest: 0 },
    }
    const bridge = scriptedBridge(
      undefined, [], undefined, undefined, undefined, undefined, undefined, undefined, undefined,
      (command, args) => {
        if (command === 'job_list') return [running, done]
        if (command === 'job_read') {
          return { job: done, chunks: [{ at: 0, text: 'hi\n', channel: 'stdout' }], next: 6, lossy: false }
        }
        if (command === 'job_kill') {
          if (args?.['jobId'] === 'bash-2') return { outcome: 'already-finished' }
          throw { code: 'job/not-found', message: '没有该作业' }
        }
        return undefined
      },
    )
    const { remote, dispose } = createAdapter({ bridge })
    const face = remote as unknown as {
      job: {
        list(request: Record<string, unknown>, signal?: AbortSignal): AsyncIterable<Record<string, unknown>>
        follow(request: Record<string, unknown>, signal?: AbortSignal): AsyncIterable<Record<string, unknown>>
        kill(request: Record<string, unknown>): Promise<Record<string, unknown>>
      }
    }

    const controller = new AbortController()
    const listed = face.job.list({ sessionId: 'agent-new' }, controller.signal)
    expect((await listed[Symbol.asyncIterator]().next()).value).toEqual({ type: 'rows', jobs: [running, done] })
    controller.abort()
    expect((await listed[Symbol.asyncIterator]().next()).done).toBe(true)

    const frames: Record<string, unknown>[] = []
    for await (const frame of face.job.follow({ sessionId: 'agent-new', jobId: 'bash-2' })) frames.push(frame)
    expect(frames.map(frame => frame['type'])).toEqual(['opened', 'output', 'status'])
    expect(frames[0]).toMatchObject({ type: 'opened', from: 0, job: { id: 'bash-2' } })
    expect(frames[1]).toMatchObject({ type: 'output', next: 6, chunks: [{ text: 'hi\n', channel: 'stdout' }] })
    expect(frames[2]).toMatchObject({ type: 'status', job: { status: 'completed' } })

    expect(await face.job.kill({ sessionId: 'agent-new', jobId: 'bash-x' })).toMatchObject({
      ok: false, error: { code: 'job/not-found' },
    })
    expect(await face.job.kill({ sessionId: 'agent-new', jobId: 'bash-2' })).toMatchObject({
      ok: true, value: { outcome: 'already-finished' },
    })
    dispose()
  })

  it('/permission：bare 回当前挡位与可用清单，未知挡位按上游文案报错', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf3(remote)
    const bare = await face.commands.execute('agent-new', '/permission')
    expect(bare).toMatchObject({
      ok: true,
      value: { result: { kind: 'success', text: expect.stringContaining('current preset') } },
    })
    expect((bare as { value: { result: { text: string } } }).value.result.text).toContain('read-only')
    const unknown = await face.commands.execute('agent-new', '/permission yolo')
    expect(unknown).toMatchObject({
      ok: true,
      value: { result: { kind: 'error', text: 'unknown preset "yolo" (available: read-only, workspace-write, danger-full-access)' } },
    })
    dispose()
  })

  it('客户端排队：运行中提交进 inbox 投影（rpcId 对账），回合收口按序自动补发', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q1', content: [{ type: 'text', text: '第一条' }] })
    // 运行中再提交：accepted 但不再调 send_task（缓存进队列）
    const queued = await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q2', content: [{ type: 'text', text: '排队第二条' }] })
    expect(queued).toMatchObject({ ok: true, value: { accepted: true } })
    expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(1)

    // 排队中的快照：inbox 行带原始 rpcId（QueueDock 行与回声对账的 join key）
    const values = await projectionValuesOf(session, 'agent-new')
    expect(values['inbox']).toEqual({
      'next-turn': [{
        id: expect.any(String), role: 'user',
        content: [{ type: 'text', text: '排队第二条' }],
        source: { kind: 'user', rpcId: 'rpc-q2' },
      }],
    })

    // 收口 → 自动补发：send_task 第二次调用 + durable user/message 回填原 rpcId
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    await vi.waitFor(async () => {
      const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
      expect(eventsOf(paged).filter(event => event['type'] === 'user/message')).toHaveLength(2)
    })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
    const second = eventsOf(paged).filter(event => event['type'] === 'user/message')[1]
    expect((second?.['data'] as { source: { rpcId?: string } }).source.rpcId).toBe('rpc-q2')
    expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(2)
    expect(bridge.calls.filter(call => call.command === 'send_task')[1]?.args).toEqual({ task: '排队第二条', agentId: 'agent-new' })
    // 队列清空：重开快照不再挂 inbox
    expect((await projectionValuesOf(session, 'agent-new'))['inbox']).toBeUndefined()
    dispose()
  })

  it('updateQueue：编辑/steer 改补发内容与顺序，移除发 spliced 事件退休回声，未知行如实拒绝', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf3(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q1', content: [{ type: 'text', text: '第一条' }] })
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q2', content: [{ type: 'text', text: '第二条' }] })
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-q3', content: [{ type: 'text', text: '第三条' }] })

    // 编辑排队的第二条（第二次排队 = shaniu-queued-1）：补发文本随之改变
    const edited = await session.updateQueue({
      sessionId: 'agent-new', itemId: 'shaniu-queued-1',
      action: { kind: 'edit', content: [{ type: 'text', text: '改过的第二条' }] },
    })
    expect(edited).toMatchObject({ ok: true, value: { accepted: true } })
    // steer 第三条（shaniu-queued-2）→ 移到队首（当前回合收口后第一个补发；行保持可见）
    const steered = await session.updateQueue({ sessionId: 'agent-new', itemId: 'shaniu-queued-2', action: { kind: 'steer' } })
    expect(steered).toMatchObject({ ok: true })
    // 移除第二条 → journal 记 agent/inbox/spliced{outcome:canceled}，客户端据此退休回声行
    const removed = await session.updateQueue({ sessionId: 'agent-new', itemId: 'shaniu-queued-1', action: { kind: 'remove' } })
    expect(removed).toMatchObject({ ok: true })

    // 未知行 / 空编辑：如实拒绝而不是静默成功（补发收口前做，排队项还都在）
    const missing = await session.updateQueue({ sessionId: 'agent-new', itemId: 'nope', action: { kind: 'remove' } })
    expect(missing['ok']).toBe(false)
    expect((missing['error'] as { code: string }).code).toBe('session/queue-item-not-found')
    const blank = await session.updateQueue({
      sessionId: 'agent-new', itemId: 'shaniu-queued-2',
      action: { kind: 'edit', content: [{ type: 'text', text: '   ' }] },
    })
    expect(blank['ok']).toBe(false)
    expect((blank['error'] as { code: string }).code).toBe('gateway/bad-request')

    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })
    await vi.waitFor(() => {
      expect(bridge.calls.filter(call => call.command === 'send_task')).toHaveLength(2)
    })
    // 补发的是 steer 到队首的第三条；被移除的第二条永远不会发出
    expect(bridge.calls.filter(call => call.command === 'send_task')[1]?.args)
      .toEqual({ task: '第三条', agentId: 'agent-new' })
    const paged = (await session.page({ sessionId: 'agent-new' }))['value'] as { records: { event: Record<string, unknown> }[] }
    const spliced = eventsOf(paged).find(event => event['type'] === 'agent/inbox/spliced')
    expect(spliced).toBeDefined()
    // 这条事件走客户端运行时会过的同一道 wire 闸
    expect(() => assertSessionWireEvent(spliced!)).not.toThrow()
    expect((spliced!['data'] as { outcome: string; removedCount: number }).outcome).toBe('canceled')
    expect((spliced!['data'] as { removedCount: number }).removedCount).toBe(1)
    dispose()
  })
})

describe('todos 投影（SH-108：todo_write → journal todo/write → TodoDock 数据面）', () => {
  interface TodoTestFace {
    session: SessionFace & {
      control(): AsyncGenerator<Record<string, unknown>>
    }
  }

  const faceOfTodo = (remote: unknown): TodoTestFace => remote as TodoTestFace

  const TODO_ARGS = {
    todos: [
      { content: ' 调研现状 ', status: 'completed' },
      { content: '写实现', status: 'in_progress' },
    ],
  }
  /** trim 后的整表（上游 toTodoList 语义：入库存 trim 结果）。 */
  const TRIMMED_TODOS = [
    { content: '调研现状', status: 'completed' },
    { content: '写实现', status: 'in_progress' },
  ]

  it('todo_write 执行成功：todo/write 落在 tool/call 与 tool/result 之间，投影带 trim 后整表，事件过 wire 闸', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t1', content: [{ type: 'text', text: '列个清单' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })

    const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    expect(journal.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'tool/call', 'todo/write', 'tool/result',
    ])
    const write = journal.find(event => event['type'] === 'todo/write')
    expect((write!['data'] as { todos: unknown }).todos).toEqual(TRIMMED_TODOS)
    // 这条事件走客户端运行时会过的同一道 wire 闸
    expect(() => assertSessionWireEvent(write!)).not.toThrow()
    // 投影整表替换：TodoDock 从这里拿渲染数据
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toEqual(TRIMMED_TODOS)
    await iterator.return?.(undefined)
    dispose()
  })

  it('失败的调用不落清单：result 非 ok 不 emit，参数不可信时即便 ok 也拒绝推导', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t2', content: [{ type: 'text', text: '列个清单' }] })

    // 校验被拒（后端 Err 路径）→ ToolCallResult::Error → 不落
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw2', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw2', exit_code: null, error: 'invalid todos: duplicate content "调研现状"', result: 'error' })
    let snapshot = (await session.follow({ address: { kind: 'session', sessionId: 'agent-new' } }).next()).value as Record<string, unknown>
    expect(eventsOf(snapshot).some(event => event['type'] === 'todo/write')).toBe(false)
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toBeUndefined()

    // 防御位：执行结果 ok 但参数不是对象形状（协议违规）→ 一样不落，不伪造清单
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw3', tool: 'todo_write', args: 'not-an-object' })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw3', exit_code: 0, error: null, result: 'ok' })
    snapshot = (await session.follow({ address: { kind: 'session', sessionId: 'agent-new' } }).next()).value as Record<string, unknown>
    expect(eventsOf(snapshot).filter(event => event['type'] === 'todo/write')).toHaveLength(0)
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toBeUndefined()
    dispose()
  })

  it('回合边界：turn/end 保留已完成的清单可见，下一次 turn/start 清空（显式 null 帧 + 快照缺席）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOfTodo(remote).session
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t3a', content: [{ type: 'text', text: '第一件事' }] })
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    // turn/end 不清空：已完成的清单在回合结束后仍可见（上游 fold 注释的语义）
    const control = session.control()
    const baseline = (await control.next()).value as { value: { projections: Record<string, { values: Record<string, unknown> }> } }
    expect(baseline.value.projections['agent-new']!.values['todos']).toEqual(TRIMMED_TODOS)

    // 新回合（turn/start）清空：同一客户端收到显式 null 帧
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-t3b', content: [{ type: 'text', text: '第二件事' }] })
    const frame = (await control.next()).value as { changes: { key: string; val: unknown }[] }
    expect(frame.changes).toEqual([{ key: 'todos', val: null }])
    await control.return?.(undefined)
    // 新观察者从快照进来：todos 键缺席（投影缺席 = TodoDock 不渲染）
    const values = await (async () => {
      const iterator = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
      const snap = (await iterator.next()).value as Record<string, unknown>
      await iterator.return?.(undefined)
      return (snap['projections'] as { values: Record<string, unknown> }).values
    })()
    expect(values['todos']).toBeUndefined()
    dispose()
  })

  it('重开旧会话：清单从档案重建（trim 镜像同源），校验被拒的调用不伪造', async () => {
    const archive = [
      { kind: 'message', role: 'user', text: '列个清单' },
      { kind: 'tool_call', call_id: 'tw1', tool: 'todo_write', arguments: JSON.stringify(TODO_ARGS), output: 'Updated todo list: 0 pending, 1 in progress, 1 completed.\n[exit code: 0]' },
      // 后端校验拒绝的调用只留错误文本（Err 路径无 exit 标记）：镜像谓词重演同一裁决
      { kind: 'tool_call', call_id: 'tw2', tool: 'todo_write', arguments: '{"todos":[{"content":"x","status":"pending"},{"content":"x","status":"pending"}]}', output: '参数非法: invalid todos: duplicate content "x"' },
      { kind: 'message', role: 'assistant', text: '清单好了' },
    ]
    const bridge = scriptedBridge(undefined, archive)
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOfTodo(remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const snapshot = (await iterator.next()).value as Record<string, unknown>
    const journal = eventsOf(snapshot)
    const types = journal.map(event => event['type'])
    // todo/write 紧跟第一份调用的 tool/call、先于 tool/result；第二份调用没有 todo/write
    expect(types.filter(type => type === 'todo/write')).toHaveLength(1)
    const writeIndex = types.indexOf('todo/write')
    expect(types[writeIndex - 1]).toBe('tool/call')
    expect(types[writeIndex + 1]).toBe('tool/result')
    expect((journal[writeIndex]!['data'] as { todos: unknown }).todos).toEqual(TRIMMED_TODOS)
    journal.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    // 投影随快照带出：重开旧会话 TodoDock 直接回到最后一份清单
    expect((snapshot['projections'] as { values: Record<string, unknown> }).values['todos']).toEqual(TRIMMED_TODOS)
    await iterator.return?.(undefined)
    dispose()
  })

  it('同一份历史：todo 的实时序列与重开重建序列逐项一致（含 todo/write 载荷）', async () => {
    const liveBridge = scriptedBridge()
    const live = createAdapter({ bridge: liveBridge })
    const liveSession = faceOfTodo(live.remote).session
    await liveSession.prompt({ sessionId: 'agent-new', requestId: 'rpc-teq', content: [{ type: 'text', text: '第一步' }] })
    liveBridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'tw1', tool: 'todo_write', args: TODO_ARGS })
    liveBridge.emit('agent-new', { type: 'tool_call_end', call_id: 'tw1', exit_code: 0, error: null, result: 'ok' })
    liveBridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' })

    const liveIterator = liveSession.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const liveJournal = eventsOf((await liveIterator.next()).value as Record<string, unknown>)
    await liveIterator.return?.(undefined)
    live.dispose()

    const archive = [
      { kind: 'message', role: 'user', text: '第一步' },
      { kind: 'tool_call', call_id: 'tw1', tool: 'todo_write', arguments: JSON.stringify(TODO_ARGS), output: '[exit code: 0]' },
    ]
    const reopened = createAdapter({ bridge: scriptedBridge(undefined, archive) })
    const replayIterator = faceOfTodo(reopened.remote).session.follow({ address: { kind: 'session', sessionId: 'agent-old' } })
    const replayJournal = eventsOf((await replayIterator.next()).value as Record<string, unknown>)
    await replayIterator.return?.(undefined)
    reopened.dispose()

    expect(replayJournal.map(event => event['type'])).toEqual(liveJournal.map(event => event['type']))
    const liveWrite = liveJournal.find(event => event['type'] === 'todo/write')
    const replayWrite = replayJournal.find(event => event['type'] === 'todo/write')
    expect(replayWrite!['data']).toEqual(liveWrite!['data'])
  })
})

describe('workspace 面：归档/置顶/手动排序（SH-100）', () => {
  interface WorkspaceTestFace {
    workspace: {
      follow(): AsyncGenerator<Record<string, unknown>>
      archiveSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      unarchiveSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      pinSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      unpinSession(request: Record<string, unknown>): Promise<Record<string, unknown>>
      insertSessionBefore(request: Record<string, unknown>): Promise<Record<string, unknown>>
    }
  }

  const faceOfWorkspace = (remote: unknown): WorkspaceTestFace => remote as WorkspaceTestFace

  it('baseline 带归档/置顶集合；归档推 archived 全量替换帧', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    expect(baseline['archivedSessionIds']).toEqual([])
    expect(baseline['pinnedSessionIds']).toEqual([])

    const result = await face.workspace.archiveSession({ sessionId: 'agent-new' })
    expect(result['ok']).toBe(true)
    expect((result['value'] as { archivedSessionIds: string[] }).archivedSessionIds).toEqual(['agent-new'])
    const frame = (await iterator.next()).value as { type: string; archivedSessionIds: string[] }
    expect(frame.type).toBe('archived')
    expect(frame.archivedSessionIds).toEqual(['agent-new'])
    await iterator.return?.(undefined)
    dispose()
  })

  it('置顶 prepend（最近置顶在前）推 pinned 帧；归档即掉置顶（同一次写）', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.pinSession({ sessionId: 'a' })
    await face.workspace.pinSession({ sessionId: 'b' })
    // 已置顶的幂等不重排
    await face.workspace.pinSession({ sessionId: 'a' })
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    // prepend：最近置顶在前（上游 pinnedSessionIds 的 unshift 语义）
    expect(baseline['pinnedSessionIds']).toEqual(['b', 'a'])

    // 已置顶的会话归档：archived 与 pinned 两帧都推（一次写里两个集合都变了）
    await face.workspace.archiveSession({ sessionId: 'a' })
    const frames: Record<string, unknown>[] = []
    for (let i = 0; i < 2; i += 1) frames.push((await iterator.next()).value as Record<string, unknown>)
    expect(frames.map(frame => frame['type']).sort()).toEqual(['archived', 'pinned'])
    const pinned = frames.find(frame => frame['type'] === 'pinned') as { pinnedSessionIds: string[] }
    expect(pinned.pinnedSessionIds).toEqual(['b'])
    await iterator.return?.(undefined)
    dispose()
  })

  it('错误按类映射 wire 码：运行中/不存在/置顶已归档；stopActivity 先取消再归档', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    const active = await face.workspace.archiveSession({ sessionId: 'agent-running' })
    expect(active['ok']).toBe(false)
    expect((active['error'] as { code: string }).code).toBe('workspace/session-active')
    expect((active['error'] as Error).message).toContain('agent-running')

    const ghost = await face.workspace.archiveSession({ sessionId: 'agent-ghost' })
    expect((ghost['error'] as { code: string }).code).toBe('session/not-found')

    // stopActivity=true：宿主先取消再归档，一次成功
    const stopped = await face.workspace.archiveSession({ sessionId: 'agent-running', stopActivity: true })
    expect(stopped['ok']).toBe(true)
    expect((stopped['value'] as { archivedSessionIds: string[] }).archivedSessionIds).toContain('agent-running')

    // 已归档的会话置顶：gateway/bad-request
    const pinArchived = await face.workspace.pinSession({ sessionId: 'agent-running' })
    expect(pinArchived['ok']).toBe(false)
    expect((pinArchived['error'] as { code: string }).code).toBe('gateway/bad-request')
    dispose()
  })

  it('insertSessionBefore：insertBefore 式移动推 upsert 帧；锚点缺失按 move-invalid', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.insertSessionBefore({ sessionId: 'agent-new' })
    const forked = await face.workspace.insertSessionBefore({ sessionId: 'agent-fork', beforeSessionId: 'agent-new' })
    expect(forked['ok']).toBe(true)
    expect((forked['value'] as { sessionOrder: string[] }).sessionOrder).toEqual(['agent-fork', 'agent-new'])

    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    const view = (baseline['items'] as { sessionIds: string[] }[])[0]!
    // 视图顺序按侧车序排：现存才进账户，fork 在 new 前面
    expect(view['sessionIds']).toEqual(['agent-new'])
    await iterator.return?.(undefined)

    const badAnchor = await face.workspace.insertSessionBefore({ sessionId: 'agent-new', beforeSessionId: 'agent-ghost' })
    expect(badAnchor['ok']).toBe(false)
    expect((badAnchor['error'] as { code: string }).code).toBe('session/not-found')
    dispose()
  })

  it('取消归档/取消置顶：集合回到空，帧随之推送', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfWorkspace(remote)
    await face.workspace.archiveSession({ sessionId: 'agent-new' })
    await face.workspace.pinSession({ sessionId: 'agent-fork' })
    await face.workspace.unarchiveSession({ sessionId: 'agent-new' })
    await face.workspace.unpinSession({ sessionId: 'agent-fork' })
    const iterator = face.workspace.follow()
    const baseline = ((await iterator.next()).value as { value: Record<string, unknown> }).value
    expect(baseline['archivedSessionIds']).toEqual([])
    expect(baseline['pinnedSessionIds']).toEqual([])
    await iterator.return?.(undefined)
    dispose()
  })
})

describe('workspace 注册表对齐上游：默认项可改可删（2026-11-07）', () => {
  /** 注册表面（create/rename/delete/follow）的窄声明。 */
  interface WorkspaceRegistryFace {
    workspace: {
      follow(): AsyncGenerator<Record<string, unknown>>
      create(request: { path?: string }): Promise<Record<string, unknown>>
      rename(request: { workspaceId?: string; title?: string }): Promise<Record<string, unknown>>
      delete(request: { workspaceId?: string }): Promise<Record<string, unknown>>
      insertBefore(request: { workspaceId?: string; beforeWorkspaceId?: string }): Promise<Record<string, unknown>>
      initializeDefault(): Promise<Record<string, unknown>>
    }
  }
  const faceOfRegistry = (remote: unknown): WorkspaceRegistryFace => remote as WorkspaceRegistryFace

  const baselineOf = async (iterator: AsyncGenerator<Record<string, unknown>>): Promise<Record<string, unknown>[]> => {
    const frame = (await iterator.next()).value as { value: { items?: Record<string, unknown>[] } }
    return frame.value['items'] ?? []
  }

  it('baseline 默认项标题来自 [workspace].title（上游 registry 可写 title 同构）', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, workspaceTitle: '主力', workspaces: [{ id: 'ws-x', path: 'X', title: 'X' }] })
    const { remote, dispose } = createAdapter({ bridge })
    const iterator = faceOfRegistry(remote).workspace.follow()
    const items = await baselineOf(iterator)
    expect(items[0]?.['workspaceId']).toBe('default-workspace')
    expect(items[0]?.['title']).toBe('主力')
    await iterator.return?.(undefined)
    dispose()
  })

  it('改默认项名：upsert 帧带新标题；重启基线从 [workspace].title 同源读出', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfRegistry(remote)
    const iterator = face.workspace.follow()
    await iterator.next()
    const result = await face.workspace.rename({ workspaceId: 'default-workspace', title: '主力' })
    expect(result['ok']).toBe(true)
    const frame = (await iterator.next()).value as { type: string; workspace: { title: string } }
    expect(frame.type).toBe('upsert')
    expect(frame.workspace['title']).toBe('主力')
    await iterator.return?.(undefined)
    // 基线重拉：标题基线随 config（face.refresh 后）而不是旧缓存
    const second = faceOfRegistry(remote).workspace.follow()
    const items = await baselineOf(second)
    expect(items[0]?.['title']).toBe('主力')
    await second.return?.(undefined)
    dispose()
  })

  it('删除默认项：remove 帧推给前端，重启基线不再含默认项', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, workspaces: [{ id: 'ws-x', path: 'X', title: 'X' }] })
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfRegistry(remote)
    const iterator = face.workspace.follow()
    const items = await baselineOf(iterator)
    expect(items.map(item => item['workspaceId'])).toEqual(['default-workspace', 'ws-x'])
    const result = await face.workspace.delete({ workspaceId: 'default-workspace' })
    expect(result['ok']).toBe(true)
    const frame = (await iterator.next()).value as { type: string; workspaceId: string }
    expect(frame.type).toBe('remove')
    expect(frame.workspaceId).toBe('default-workspace')
    await iterator.return?.(undefined)
    const second = faceOfRegistry(remote).workspace.follow()
    const rebaseline = await baselineOf(second)
    expect(rebaseline.map(item => item['workspaceId'])).toEqual(['ws-x'])
    await second.return?.(undefined)
    dispose()
  })

  /**
   * 驱动**真实 vendored 客户端模型**（SH-110/112 的验收要求穿透这一层）：
   * `removedIds` 不变式与 `initializeDefault` 的 undefined 判定都在它里面，
   * 只断言 adapter 回值测不到「回声被静默丢弃」这类真机缺陷。
   */
  interface LooseWorkspaceModel {
    initializeDefault(): Promise<{ ok: boolean; value?: { workspace: Record<string, unknown> } }>
    create(input: { path: string }): Promise<{ ok: boolean; value: { created: boolean; workspace: Record<string, unknown> } }>
    delete(id: string): Promise<{ ok: boolean }>
    insertBefore(id: string, before?: string): Promise<{ ok: boolean; value: { workspaceIds: string[] } }>
    replaceBaseline(value: { items: Record<string, unknown>[]; archivedSessionIds: string[]; pinnedSessionIds: string[] }): void
    getSnapshot(): { items: { workspaceId: string; path: string }[] }
  }
  const modelOf = (remote: unknown): LooseWorkspaceModel =>
    new ClientWorkspaceModel(
      faceOfRegistry(remote).workspace as unknown as ConstructorParameters<typeof ClientWorkspaceModel>[0],
    ) as unknown as LooseWorkspaceModel

  it('删除默认项后同路径重新添加：全新 id 穿透 removedIds，回声与基线都可见（SH-110）', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, workspaces: [] })
    const { remote, dispose } = createAdapter({ bridge })
    const model = modelOf(remote)
    await model.initializeDefault()
    expect(model.getSnapshot().items.map(item => item.workspaceId)).toEqual(['default-workspace'])
    await model.delete('default-workspace')
    expect(model.getSnapshot().items).toEqual([])
    // 同路径重新添加：宿主全新注册（created:true + 新 id），不复用被删的固定 id
    const created = await model.create({ path: 'W' })
    expect(created.ok).toBe(true)
    expect(created.value.created).toBe(true)
    const revived = model.getSnapshot().items
    expect(revived).toHaveLength(1)
    expect(revived[0]?.workspaceId).not.toBe('default-workspace')
    expect(revived[0]?.path).toBe('W')
    // 基线同样不能被 removedIds 丢弃（换代/重开窗口走的是这条路）
    const iterator = faceOfRegistry(remote).workspace.follow()
    const items = await baselineOf(iterator)
    model.replaceBaseline({ items, archivedSessionIds: [], pinnedSessionIds: [] })
    expect(model.getSnapshot().items.map(item => item.workspaceId)).toEqual([revived[0]?.workspaceId])
    await iterator.return?.(undefined)
    dispose()
  })

  it('空注册表重启：initializeDefault 回裸 undefined，vendored model 不抛 TypeError（SH-112）', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, workspaceHidden: true, workspaces: [] })
    const { remote, dispose } = createAdapter({ bridge })
    const model = modelOf(remote)
    // 旧实现回 {workspace: undefined}：model 判 value !== undefined 成立 →
    // upsert(undefined) 抛 TypeError → navigation 兜底成「默认工作区初始化失败」
    const result = await model.initializeDefault()
    expect(result.ok).toBe(true)
    expect(result.value).toBeUndefined()
    expect(model.getSnapshot().items).toEqual([])
    // hidden 是用户显式删除的事实，启动不复活（上游 deleteKnown 不清 defaultWorkspaceId，
    // 实体已删 → initializeDefault 恒 undefined）
    expect(bridge.calls.some(call => call.command === 'create_workspace')).toBe(false)
    dispose()
  })

  it('默认项可见时 initializeDefault 按 id 找它，不取顺序首项（SH-111 排序后仍成立）', async () => {
    const bridge = scriptedBridge({
      ...HOST_SETTINGS,
      workspaces: [{ id: 'ws-a', path: 'A', title: 'A' }],
      workspaceOrder: ['ws-a', 'default-workspace'],
    })
    const { remote, dispose } = createAdapter({ bridge })
    const result = await faceOfRegistry(remote).workspace.initializeDefault()
    expect(result['ok']).toBe(true)
    expect((result['value'] as { workspace: { workspaceId: string } }).workspace.workspaceId).toBe('default-workspace')
    dispose()
  })

  it('拖拽排序：回值=完整顺序、推 order 帧、重启基线与客户端模型都按新顺序（SH-111）', async () => {
    const bridge = scriptedBridge({
      ...HOST_SETTINGS,
      workspaces: [{ id: 'ws-a', path: 'A', title: 'A' }, { id: 'ws-b', path: 'B', title: 'B' }],
    })
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfRegistry(remote)
    const iterator = face.workspace.follow()
    const baseline = await baselineOf(iterator)
    expect(baseline.map(item => item['workspaceId'])).toEqual(['default-workspace', 'ws-a', 'ws-b'])
    const model = modelOf(remote)
    // 模型先装上基线才有 items 可排（installOrder 排的是已投影的行）
    model.replaceBaseline({ items: baseline, archivedSessionIds: [], pinnedSessionIds: [] })
    const moved = await model.insertBefore('ws-b', 'default-workspace')
    expect(moved.ok).toBe(true)
    expect(moved.value.workspaceIds).toEqual(['ws-b', 'default-workspace', 'ws-a'])
    // 客户端模型装上 Host 确认的完整顺序（不是只有被移动那一家）
    expect(model.getSnapshot().items.map(item => item.workspaceId)).toEqual(['ws-b', 'default-workspace', 'ws-a'])
    const frame = (await iterator.next()).value as { type: string; workspaceIds: string[] }
    expect(frame.type).toBe('order')
    expect(frame.workspaceIds).toEqual(['ws-b', 'default-workspace', 'ws-a'])
    await iterator.return?.(undefined)
    // 重启 = 新 adapter 读同一份宿主文档：顺序持久，不回落 config 数组序
    const restarted = createAdapter({ bridge })
    const second = faceOfRegistry(restarted.remote).workspace.follow()
    expect((await baselineOf(second)).map(item => item['workspaceId'])).toEqual(['ws-b', 'default-workspace', 'ws-a'])
    await second.return?.(undefined)
    restarted.dispose()
    dispose()
  })

  it('拖拽排序的非法锚点按 workspace/not-found 回显，details 带 workspaceId（SH-111）', async () => {
    const bridge = scriptedBridge({ ...HOST_SETTINGS, workspaces: [{ id: 'ws-a', path: 'A', title: 'A' }] })
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOfRegistry(remote)
    const result = await face.workspace.insertBefore({ workspaceId: 'ws-a', beforeWorkspaceId: 'ws-gone' })
    expect(result['ok']).toBe(false)
    const error = result['error'] as { code: string; details: Record<string, unknown> }
    expect(error.code).toBe('workspace/not-found')
    expect(error.details['workspaceId']).toBe('ws-a')
    // 失败不写宿主顺序
    expect(bridge.calls.filter(call => call.command === 'insert_workspace_before')).toHaveLength(1)
    dispose()
  })
})

describe('子代理泳道（SH-109）：AgentEvent 信封的 agent_id 路由', () => {
  /** 泳道面的窄声明：follow + 控制流基线。 */
  interface ControlFace {
    session: SessionFace & {
      control(): AsyncGenerator<{ value: { projections: Record<string, { values: Record<string, unknown> }> } }>
    }
  }

  it('懒建泳道：非 root 事件路由进泳道 journal，父 journal 只落 catalog 引用事件', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session as ControlFace['session']
    await session.prompt({ sessionId: 'agent-new', requestId: 'rpc-1', content: [{ type: 'text', text: '派子代理去探索' }] })
    // 后端 spawn 后子代理的事件带 agent_id（session_id 仍是宿主任务会话）
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '我去派子代理' })
    bridge.emit('agent-new', { type: 'turn_started', turn: 1 }, 'lane-a')
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '开始探索' }, 'lane-a')
    bridge.emit('agent-new', { type: 'tool_call_begin', call_id: 'l1', tool: 'read_file', args: { path: 'src/transport' } }, 'lane-a')
    bridge.emit('agent-new', { type: 'tool_call_end', call_id: 'l1', exit_code: 0, error: null, result: 'ok' }, 'lane-a')

    // 泳道 journal：回合开 → 工具调用（delta 进的是助手流，不是 journal 事件）
    const lane = session.follow({ address: { kind: 'subagent', parentSessionId: 'agent-new', childSessionId: 'lane-a', mode: 'unknown' } })
    const laneSnapshot = (await lane.next()).value as Record<string, unknown>
    const laneEvents = eventsOf(laneSnapshot)
    // tool_call_begin 会先定稿此前的正文（assistant/message），再落调用
    expect(laneEvents.map(event => event['type'])).toEqual(['turn/start', 'step/start', 'assistant/message', 'tool/call', 'tool/result'])
    laneEvents.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    // 父 journal：只有 catalog 引用事件，没有泳道的工具调用——回合/步数/指标不被污染
    const parent = session.follow({ address: { kind: 'session', sessionId: 'agent-new' } })
    const parentSnapshot = (await parent.next()).value as Record<string, unknown>
    const parentEvents = eventsOf(parentSnapshot)
    expect(parentEvents.map(event => event['type'])).toEqual([
      'turn/start', 'step/start', 'user/message', 'subagent/catalog',
    ])
    expect((parentEvents.at(-1)?.['data'] as Record<string, unknown>)).toMatchObject({
      version: 1, childId: 'lane-a', mode: 'unknown',
    })
    parentEvents.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())

    // 控制基线：父挂 subagentCatalog（懒建泳道 = v1 unknown），泳道 timing 未见 descriptor 如实缺席
    const control = session.control()
    const baseline = (await control.next()).value
    expect(baseline.value.projections['agent-new']?.values['subagentCatalog']).toEqual([
      { id: 'lane-a', createdAt: expect.any(Number), mode: 'unknown' },
    ])
    expect(baseline.value.projections['lane-a']?.values['subagentTiming']).toBeUndefined()
    await control.return?.(undefined)

    await lane.return?.(undefined)
    await parent.return?.(undefined)
    dispose()
  })

  it('泳道完成态：turn/end 落定、运行状态收回，wire 规则全程过闸', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session as ControlFace['session']
    const statuses: [string, boolean][] = []
    ;(remote as unknown as {
      $on(event: string, listener: (...args: never[]) => unknown): () => void
    }).$on('api-session/status', (id: string, running: boolean) => {
      statuses.push([id, running])
      return Promise.resolve('')
    })

    bridge.emit('agent-new', { type: 'turn_started', turn: 1 }, 'lane-a')
    bridge.emit('agent-new', { type: 'agent_message_delta', delta: '结论：模块边界清晰' }, 'lane-a')
    bridge.emit('agent-new', { type: 'turn_completed', turn: 1, reason: 'completed' }, 'lane-a')

    // 运行状态的开与收都按泳道 id 广播（目录树徽标与 Stop 按钮的数据源）
    expect(statuses).toEqual([['lane-a', true], ['lane-a', false]])

    const lane = session.follow({ address: { kind: 'subagent', parentSessionId: 'agent-new', childSessionId: 'lane-a', mode: 'unknown' } })
    const snapshot = (await lane.next()).value as Record<string, unknown>
    const events = eventsOf(snapshot)
    expect(events.map(event => event['type'])).toEqual(['turn/start', 'step/start', 'assistant/message', 'step/end', 'turn/end'])
    expect((events.at(-1)?.['data'] as { reason: unknown }).reason).toEqual({ kind: 'completed' })
    events.forEach(event => expect(() => assertSessionWireEvent(event)).not.toThrow())
    await lane.return?.(undefined)
    dispose()
  })

  it('泳道的审批请求按泳道会话作用域派发（审批卡落在泳道 composer 上）', async () => {
    const bridge = scriptedBridge()
    const scopes = new Map<string, unknown>([['lane-a', { sessionId: 'lane-a' }]])
    const { remote, dispose } = createAdapter({ bridge, approvalScope: id => scopes.get(id) })
    const seen: { self: unknown; request: Record<string, unknown> }[] = []
    faceOf(remote).$on('approval/request', async function (this: unknown, request: Record<string, unknown>) {
      seen.push({ self: this, request })
      return 'allowed-once'
    })
    bridge.emit('agent-new', { type: 'exec_approval_request', request_id: 'r-l1', tool: 'apply_patch', args: { path: 'a.txt' }, risk: 'write_in_workspace' }, 'lane-a')
    await vi.waitFor(() => { expect(seen).toHaveLength(1) })
    expect(seen[0]!.self).toEqual({ sessionId: 'lane-a' })
    expect(seen[0]!.request).toMatchObject({ agent: { id: 'lane-a' }, toolName: 'apply_patch' })
    expect(bridge.calls.find(call => call.command === 'approve')?.args).toEqual({ requestId: 'r-l1', decision: 'allow' })
    dispose()
  })

  it('remote.subagents 桌面真实态按上游错误码显式拒绝，不伪造受理', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const subagents = (remote as unknown as {
      subagents: {
        prompt(request: Record<string, unknown>): Promise<never>
        interruptByUser(childSessionId: string): Promise<never>
      }
    }).subagents
    await expect(subagents.prompt({ childSessionId: 'lane-a', requestId: 'r1', delivery: 'queue', content: [] }))
      .rejects.toThrow('SH-053')
    await expect(subagents.interruptByUser('lane-a')).rejects.toThrow('SH-053')
    dispose()
  })
})

describe('session.search 接宿主 FTS5 派生索引（SH-113，wire query-only）', () => {
  it('成功映射成 wire 形状：只传 {query}，按 sessionId 去重，hasMore 原样透传', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, () => ({
      items: [
        { sessionId: 's1', snippet: '中文短语片段' },
        { sessionId: 's2', snippet: '另一段正文' },
        { sessionId: 's1', snippet: '重复会话该被丢掉' },
      ],
      hasMore: true,
    }))
    const { remote, dispose } = createAdapter({ bridge })
    const result = await faceOf(remote).session.search({ query: '中文短语' })
    expect(result['ok']).toBe(true)
    const value = result['value'] as { items: { sessionId: string; snippet: string }[]; hasMore: boolean }
    expect(value.items.map(item => item.sessionId)).toEqual(['s1', 's2'])
    expect(value.items[0]?.snippet).toBe('中文短语片段')
    // hasMore 只是「结果较多，请收窄查询」提示，适配层不据此发起续页。
    expect(value.hasMore).toBe(true)
    const calls = bridge.calls.filter(call => call.command === 'search_sessions')
    expect(calls).toHaveLength(1)
    expect(calls[0]?.args).toEqual({ query: '中文短语' })
    dispose()
  })

  it('宿主索引/档案失败按 wire 契约回 {ok:false}，不让 rejection 穿透', async () => {
    const failing = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, () => {
      throw { code: 'index-failed', message: '索引打不开' }
    })
    const { remote, dispose } = createAdapter({ bridge: failing })
    const result = await faceOf(remote).session.search({ query: '中文' })
    expect(result['ok']).toBe(false)
    const error = result['error'] as { message?: string } | undefined
    expect(error?.message).toContain('索引打不开')
    // 只发一次：不静默重试、不返回空表冒充"没搜到"。
    expect(failing.calls.filter(call => call.command === 'search_sessions')).toHaveLength(1)
    dispose()

    const archiveFailure = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, () => {
      throw { code: 'archive-read-failed', message: '会话档案读不回' }
    })
    const { remote: remote2, dispose: dispose2 } = createAdapter({ bridge: archiveFailure })
    const result2 = await faceOf(remote2).session.search({ query: '中文' })
    expect(result2['ok']).toBe(false)
    const error2 = result2['error'] as { message?: string } | undefined
    expect(error2?.message).toContain('会话档案读不回')
    dispose2()
  })

  it('空/纯空白查询按上游回空表，不调宿主', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    for (const request of [{ query: '' }, { query: '   ' }, {}]) {
      const result = await session.search(request)
      expect(result['ok']).toBe(true)
      const value = result['value'] as { items: unknown[]; hasMore: boolean }
      expect(value.items).toEqual([])
      expect(value.hasMore).toBe(false)
    }
    expect(bridge.calls.filter(call => call.command === 'search_sessions')).toHaveLength(0)
    dispose()
  })

  it('hasMore 不是续页：同名 query 再搜也只带 {query}，不触发 Show more 请求', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, () => ({
      items: [{ sessionId: 's1', snippet: '片段' }], hasMore: true,
    }))
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session
    const first = await session.search({ query: '中文' })
    const second = await session.search({ query: '中文' })
    const calls = bridge.calls.filter(call => call.command === 'search_sessions')
    expect(calls).toHaveLength(2)
    expect(calls.map(call => call.args)).toEqual([{ query: '中文' }, { query: '中文' }])
    const firstValue = first['value'] as { items: unknown[]; hasMore: boolean }
    const secondValue = second['value'] as { items: unknown[]; hasMore: boolean }
    expect(secondValue.items).toEqual(firstValue.items)
    expect(secondValue.hasMore).toBe(true)
    dispose()
  })

  // ── SH-092：文档预览真实读取（workspaceFiles 接宿主） ──────────────────────────
  it('SH-092 文本读：顶层参数正好是 camelCase，宿主返回原样交出', async () => {
    const body = { absolutePath: 'C:/w/a.txt', version: '7-3', bytes: 3, offset: 2, text: 'x\ny', lines: 2, eof: true }
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, () => body)
    const { remote, dispose } = createAdapter({ bridge })
    const files = faceOf(remote).workspaceFiles as unknown as Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    const result = await files['read']!('sess-1', 'a.txt', { offset: 2, limit: 50 })
    const call = bridge.calls.find(item => item.command === 'workspace_file_read')
    expect(Object.keys(call?.args ?? {})).toEqual(['sessionId', 'path', 'offset', 'limit'])
    expect(Object.keys(call?.args ?? {}).some(key => key.includes('_'))).toBe(false)
    expect(result['value']).toEqual(body)
    dispose()
  })

  it('SH-092 字节读：base64 还原成 Uint8Array，元数据字段不丢', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined,
      () => ({ absolutePath: 'C:/w/p.png', version: '9-5', bytes: 5, offset: 0, data: 'aGVsbG8=', eof: true }))
    const { remote, dispose } = createAdapter({ bridge })
    const files = faceOf(remote).workspaceFiles as unknown as Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    const result = await files['readBytes']!('sess-1', 'p.png', { range: { offset: 0, length: 5 } })
    const value = result['value'] as { data: Uint8Array; bytes: number; eof: boolean; absolutePath: string }
    expect(Array.from(value.data)).toEqual([104, 101, 108, 108, 111])
    expect(value.bytes).toBe(5)
    expect(value.eof).toBe(true)
    const call = bridge.calls.find(item => item.command === 'workspace_file_read_bytes')
    expect(call?.args).toEqual({ sessionId: 'sess-1', path: 'p.png', offset: 0, length: 5 })
    dispose()
  })

  it('SH-092 类型化失败按码翻译，details 带上越界路径与上限', async () => {
    const outside = { code: 'workspace-file/outside-workspace', message: '越界', details: { path: 'C:/outside' } }
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, () => { throw outside })
    const { remote, dispose } = createAdapter({ bridge })
    const files = faceOf(remote).workspaceFiles as unknown as Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>
    const failed = await files['stat']!('sess-1', 'C:/outside/x.txt')
    expect(failed['ok']).toBe(false)
    expect((failed['error'] as { code: string }).code).toBe('workspace-file/outside-workspace')
    expect((failed['error'] as { details: Record<string, unknown> }).details['path']).toBe('C:/outside')
    const large = { code: 'workspace-file/too-large', message: '太大', details: { path: 'big.txt', limit: 1048576 } }
    const other = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, () => { throw large })
    const second = createAdapter({ bridge: other })
    const result = await (faceOf(second.remote).workspaceFiles as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>)['read']!('sess-1', 'big.txt')
    expect((result['error'] as { details: Record<string, unknown> }).details['limit']).toBe(1048576)
    // 认不出的码不猜语义：如实回落 gateway/internal
    const weird = { code: 'workspace-file/未来码', message: '不认识', details: {} }
    const third = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, () => { throw weird })
    const adapter3 = createAdapter({ bridge: third })
    const fallback = await (faceOf(adapter3.remote).workspaceFiles as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>)['list']!('sess-1', '')
    expect((fallback['error'] as { code: string }).code).toBe('gateway/internal')
    dispose()
    second.dispose()
    adapter3.dispose()
  })

  it('SH-092 批3 打开方式：应用列表透传、显式应用随 open 请求下发、reveal 不带应用', async () => {
    const apps = [{ id: 'C:/Apps/Code.exe', name: 'Visual Studio Code', default: true, icon: null }]
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, undefined,
      command => (command === 'workspace_path_applications' ? apps : undefined))
    const { remote, dispose } = createAdapter({ bridge })
    const listed = await (faceOf(remote).session as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>)['workspacePathApplications']!({ path: 'C:/ws/a.ts' })
    expect(listed['value']).toEqual(apps)
    expect(bridge.calls.find(call => call.command === 'workspace_path_applications')?.args).toEqual({ path: 'C:/ws/a.ts' })

    const opened = await (faceOf(remote).session as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>)['openWorkspacePath']!({ path: 'C:/ws/a.ts', application: 'C:/Apps/Code.exe' })
    expect(opened['value']).toEqual({ opened: true })
    expect(bridge.calls.find(call => call.command === 'open_workspace_path')?.args).toEqual({ path: 'C:/ws/a.ts', application: 'C:/Apps/Code.exe' })

    await (faceOf(remote).session as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>)['openWorkspacePath']!({ path: 'C:/ws/a.ts', action: 'reveal' })
    const revealCalls = bridge.calls.filter(call => call.command === 'open_workspace_path')
    expect(revealCalls.at(-1)?.args).toEqual({ path: 'C:/ws/a.ts', action: 'reveal' })
    expect(revealCalls.at(-1)?.args && Object.keys(revealCalls.at(-1)!.args!)).not.toContain('application')
    dispose()
  })

  it('SH-090 @file 候选：命令参数 camelCase、结果透传、会话候选如实空表', async () => {
    const candidates = [{ path: 'src/main.ts', kind: 'file' }, { path: 'src', kind: 'directory' }]
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, undefined,
      command => (command === 'list_file_references' ? candidates : undefined))
    const { remote, dispose } = createAdapter({ bridge })
    const files = (faceOf(remote) as unknown as Record<string, Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>>).fileReferences
    const listed = await files['list']!('sess-1', 'main')
    expect(listed['value']).toEqual(candidates)
    const call = bridge.calls.find(item => item.command === 'list_file_references')
    expect(call?.args).toEqual({ sessionId: 'sess-1', query: 'main' })
    expect(Object.keys(call?.args ?? {}).some(key => key.includes('_'))).toBe(false)
    const sessionCandidates = [{ sessionId: 'sess-2', label: '旧会话', displayTitle: '旧会话', sameWorkspace: true, createdAt: 1, mention: '@[旧会话](dsh-session:abc)' }]
    const bridgeWithSessions = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, undefined,
      command => (command === 'list_session_references' ? sessionCandidates : undefined))
    const second = createAdapter({ bridge: bridgeWithSessions })
    const resolver = (faceOf(second.remote) as unknown as Record<string, Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>>).sessionReferenceResolver
    const sessions = await resolver['candidates']!('sess-1', '旧')
    expect(sessions['value']).toEqual(sessionCandidates)
    const sessionCall = bridgeWithSessions.calls.find(item => item.command === 'list_session_references')
    expect(sessionCall?.args).toEqual({ sessionId: 'sess-1', query: '旧' })
    second.dispose()
    dispose()
  })

  it('SH-090 @file 候选：宿主失败回 {ok:false} 而不是空表冒充没搜到', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, undefined,
      () => { throw { message: '目录不可读' } })
    const { remote, dispose } = createAdapter({ bridge })
    const files = (faceOf(remote) as unknown as Record<string, Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>>).fileReferences
    const failed = await files['list']!('sess-1', 'main')
    expect(failed['ok']).toBe(false)
    expect((failed['error'] as { message: string }).message).toContain('目录不可读')
    dispose()
  })

  it('SH-092 批3 打开方式：宿主拒单回 {ok:false}，不让 rejection 穿透', async () => {
    const bridge = scriptedBridge(HOST_SETTINGS, [], undefined, undefined, undefined, undefined, undefined,
      () => { throw { message: '应用不可用' } })
    const { remote, dispose } = createAdapter({ bridge })
    const session = faceOf(remote).session as unknown as Record<string, (...a: unknown[]) => Promise<Record<string, unknown>>>
    const failed = await session['openWorkspacePath']!({ path: 'C:/ws/a.ts', application: 'C:/gone.exe' })
    expect(failed['ok']).toBe(false)
    expect((failed['error'] as { message: string }).message).toContain('应用不可用')
    const listFailed = await session['workspacePathApplications']!({ path: 'C:/ws/a.ts' })
    expect(listFailed['ok']).toBe(false)
    dispose()
  })

  it('SH-092 变更流：宿主没有监视面就带码中断，不挂一条永不发声的流', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const stream = (faceOf(remote).workspaceFiles as unknown as Record<string, (...a: unknown[]) => AsyncGenerator<unknown>>)['changes']!('sess-1', 'a.txt')
    await expect(stream.next()).rejects.toThrow(/宿主没有文件监视面/)
    dispose()
  })
})

describe('SH-040 只读空态命名空间（插件 / 自动化任务）', () => {
  it('插件不可管理、任务目录为空、写面显式拒绝', async () => {
    const bridge = scriptedBridge()
    const { remote, dispose } = createAdapter({ bridge })
    const face = faceOf(remote) as unknown as Record<string, Record<string, (...args: unknown[]) => Promise<Record<string, unknown>>>>
    const inventory = await face.pluginInventory!['list']!()
    expect(inventory['value']).toEqual({ managementAvailable: false })
    const manager = await face.pluginManager!['listBundles']!()
    expect(manager['ok']).toBe(false)
    expect((manager['error'] as { code: string }).code).toBe('plugin-manager/unavailable')
    const probe = await face.pluginRegistryProbe!['fastest']!()
    expect(probe['ok']).toBe(false)
    const catalog = await face.schedule!['catalog']!()
    expect(catalog['value']).toEqual([])
    const list = await face.schedule!['list']!()
    expect((list['value'] as { items: unknown[] }).items).toEqual([])
    const created = await face.schedule!['create']!({})
    expect(created['ok']).toBe(false)
    expect((created['error'] as { code: string }).code).toBe('schedule/unavailable')
    dispose()
  })
})
