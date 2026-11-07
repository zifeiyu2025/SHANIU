import { describe, expect, it, vi } from 'vitest'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import { SessionLogDownloadController } from '../dsh/packages/session-query/session-log-export/src/client/controller'
import type { HostBridge } from '../transport/tauri'
import {
  exportZipFetcher, exportZipSaver, serializeSessionLog, sessionLogFilename, SESSION_FORMAT_VERSION,
  type ExportHeaderLike,
} from './export-zip'
import { sessionEntriesToEvents, type SessionEntryLike } from './session-events'

const SESSION = 'sess-1' as SessionId

const HEADER: ExportHeaderLike = {
  id: 'sess-1',
  created_at: 1_700_000_000_000,
  cwd: 'D:\\Work\\demo',
  parent_session: null,
  delegation_depth: 0,
}

const ENTRIES: SessionEntryLike[] = [
  { kind: 'message', role: 'user', text: '看一下目录' },
  { kind: 'tool_call', call_id: 'c1', tool: 'list_dir', arguments: '{"path":"."}', output: 'a.ts' },
  { kind: 'message', role: 'assistant', text: '只有一个文件。' },
]

/** 记录每次命令调用的假桥：handler 缺省即断言"不该调到"。 */
function harness(handlers: Record<string, (args: Record<string, unknown>) => Promise<unknown>>) {
  const calls: Array<{ command: string; args: Record<string, unknown> }> = []
  const bridge: HostBridge = {
    async invoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
      calls.push({ command, args: args ?? {} })
      const handler = handlers[command]
      if (handler === undefined) throw new Error(`未预期的宿主命令: ${command}`)
      return await handler(args ?? {}) as T
    },
    listenEvents: () => () => {},
  }
  return { bridge, calls }
}

function names(calls: Array<{ command: string }>): string[] {
  return calls.map(c => c.command)
}

describe('canonical 会话日志', () => {
  it('文件名按版本生成，且与上游 sessionFormatLogFilename 同规则', () => {
    // 守门：上游升版时这条会红，提醒同步 SESSION_FORMAT_VERSION 常量。
    expect(SESSION_FORMAT_VERSION).toBe(4)
    expect(sessionLogFilename()).toBe('session.v4.jsonl')
    expect(sessionLogFilename(0)).toBe('session.jsonl')
    expect(sessionLogFilename(7)).toBe('session.v7.jsonl')
  })

  it('header 行字段顺序/可选性对齐上游，末行有且只有一个换行', () => {
    const events = sessionEntriesToEvents(ENTRIES, '演示')
    const text = serializeSessionLog(HEADER, events)
    const lines = text.split('\n')

    expect(text.endsWith('\n')).toBe(true)
    expect(text.endsWith('\n\n')).toBe(false)
    // 联合成一行：header + 每事件一行，最后 split 的空串来自尾换行。
    expect(lines.length).toBe(2 + events.length)

    const header = JSON.parse(lines[0]!) as Record<string, unknown>
    // 顺序即上游 serialized 顺序：type/version/id/createdAt/cwd/isSeeded/delegationDepth。
    expect(Object.keys(header)).toEqual([
      'type', 'version', 'id', 'createdAt', 'cwd', 'isSeeded', 'delegationDepth',
    ])
    expect(header.type).toBe('session')
    expect(header.version).toBe(SESSION_FORMAT_VERSION)
    expect(header.id).toBe('sess-1')
    expect(header.createdAt).toBe(HEADER.created_at)
    expect(header.isSeeded).toBe(false)
    expect(header.delegationDepth).toBe(0)

    // 每一行都必须是合法 JSON（否则导入端会读到半截事件）。
    for (const line of lines.slice(1).filter(line => line !== '')) {
      expect(() => JSON.parse(line)).not.toThrow()
    }
  })

  it('cwd / parentSession 为空时省略，子代理带 delegationDepth 1', () => {
    const text = serializeSessionLog(
      { ...HEADER, cwd: null, parent_session: 'root', delegation_depth: 1 },
      [],
    )
    const header = JSON.parse(text.trimEnd()) as Record<string, unknown>
    expect('cwd' in header).toBe(false)
    expect(header.parentSession).toBe('root')
    expect(header.delegationDepth).toBe(1)
  })
})

describe('exportZipFetcher（HEAD 探活）', () => {
  it('只接管 ZIP 路由：别的路由 404 且不打宿主', async () => {
    const { bridge, calls } = harness({ export_probe: async () => HEADER })
    const fetchLike = exportZipFetcher(bridge)

    const markdown = await fetchLike('api/session.markdown?sessionId=sess-1', { method: 'HEAD' })
    expect(markdown.ok).toBe(false)
    expect(markdown.status).toBe(404)
    expect(calls).toHaveLength(0)
  })

  it('缺 sessionId 时 404，不打宿主', async () => {
    const { bridge, calls } = harness({ export_probe: async () => HEADER })
    const response = await exportZipFetcher(bridge)('api/session.export', { method: 'HEAD' })
    expect(response.status).toBe(404)
    expect(calls).toHaveLength(0)
  })

  it('探活成功返回 200 并带上会话头', async () => {
    const { bridge, calls } = harness({ export_probe: async () => HEADER })
    const response = await exportZipFetcher(bridge)('api/session.export?sessionId=sess-1', { method: 'HEAD' })
    expect(response.ok).toBe(true)
    expect(JSON.parse(await response.text())).toEqual(HEADER)
    expect(calls).toEqual([{ command: 'export_probe', args: { sessionId: 'sess-1' } }])
  })

  it('档案读不回时把原因透给调用方（controller 会显示在 Dialog 里）', async () => {
    const { bridge } = harness({
      export_probe: async () => { throw new Error('档案读不回: 找不到会话') },
    })
    const response = await exportZipFetcher(bridge)('api/session.export?sessionId=gone', { method: 'HEAD' })
    expect(response.ok).toBe(false)
    expect(response.status).toBe(404)
    expect(await response.text()).toContain('档案读不回')
  })
})

describe('SessionLogDownloadController 集成（三态 / 并发 / 取消）', () => {
  it('成功路径：探活 → 保存对话框 → 全量档案 → 写 ZIP，Dialog 落到 success', async () => {
    const { bridge, calls } = harness({
      export_probe: async () => HEADER,
      pick_save_file: async () => 'C:\\tmp\\会话.zip',
      load_session: async () => ENTRIES,
      export_session_log: async () => ({ path: 'C:\\tmp\\会话.zip', bytes: 2048, entries: 1 }),
    })
    const controller = new SessionLogDownloadController(
      exportZipFetcher(bridge),
      exportZipSaver(bridge, () => '演示', () => {}),
    )

    await controller.download(SESSION)
    expect(controller.store.getSnapshot().bySession[SESSION]?.status).toBe('success')

    await vi.waitFor(() => {
      expect(names(calls)).toContain('export_session_log')
    })
    expect(names(calls)).toEqual([
      'export_probe', 'pick_save_file', 'export_probe', 'load_session', 'export_session_log',
    ])

    const write = calls.find(c => c.command === 'export_session_log')!
    expect(write.args.dest).toBe('C:\\tmp\\会话.zip')
    const entries = write.args.entries as Array<{ path: string; content: string }>
    expect(entries).toHaveLength(1)
    expect(entries[0]!.path).toBe('session.v4.jsonl')
    const lines = entries[0]!.content.split('\n')
    expect(JSON.parse(lines[0]!).id).toBe('sess-1')
    // 用户消息在事件里，说明走的是同一份翻译（不是空壳归档）。
    expect(entries[0]!.content).toContain('看一下目录')
    await controller.dispose()
  })

  it('探活失败：Dialog 进 error 态并带原因，不弹保存对话框', async () => {
    const { bridge, calls } = harness({
      export_probe: async () => { throw new Error('档案读不回: 文件不存在') },
    })
    const controller = new SessionLogDownloadController(
      exportZipFetcher(bridge),
      exportZipSaver(bridge, () => '演示', () => {}),
    )

    await controller.download(SESSION)
    const entry = controller.store.getSnapshot().bySession[SESSION]
    expect(entry?.status).toBe('error')
    expect(entry?.error).toContain('档案读不回')
    expect(names(calls)).toEqual(['export_probe'])
    await controller.dispose()
  })

  it('用户取消保存对话框：静默结束，不读档案、不写盘、不报错', async () => {
    const { bridge, calls } = harness({
      export_probe: async () => HEADER,
      pick_save_file: async () => null,
    })
    const warn = vi.fn()
    const controller = new SessionLogDownloadController(
      exportZipFetcher(bridge),
      exportZipSaver(bridge, () => '演示', warn),
    )

    await controller.download(SESSION)
    // save 是 fire-and-forget，给它一个宏任务把 async IIFE 跑完。
    await new Promise(resolve => setTimeout(resolve, 10))

    expect(names(calls)).toEqual(['export_probe', 'pick_save_file'])
    expect(warn).not.toHaveBeenCalled()
    // 取消不是失败：上游语义里 Dialog 已报"下载已开始"，这里不回头改状态。
    expect(controller.store.getSnapshot().bySession[SESSION]?.status).toBe('success')
    await controller.dispose()
  })

  it('同会话并发只跑一次探活（上游并发合并规则）', async () => {
    let probes = 0
    const { bridge } = harness({
      export_probe: async () => { probes += 1; return HEADER },
      pick_save_file: async () => null,
    })
    const controller = new SessionLogDownloadController(
      exportZipFetcher(bridge),
      exportZipSaver(bridge, () => '演示', () => {}),
    )

    await Promise.all([controller.download(SESSION), controller.download(SESSION)])
    expect(probes).toBe(1)
    await controller.dispose()
  })

  it('写盘失败经 warn 留痕，不产生未处理 rejection', async () => {
    const { bridge } = harness({
      export_probe: async () => HEADER,
      pick_save_file: async () => 'C:\\tmp\\x.zip',
      load_session: async () => ENTRIES,
      export_session_log: async () => { throw new Error('磁盘已满') },
    })
    const warn = vi.fn()
    const controller = new SessionLogDownloadController(
      exportZipFetcher(bridge),
      exportZipSaver(bridge, () => '演示', warn),
    )

    await controller.download(SESSION)
    await vi.waitFor(() => { expect(warn).toHaveBeenCalled() })
    expect(String(warn.mock.calls[0]?.[1])).toContain('磁盘已满')
    await controller.dispose()
  })
})
