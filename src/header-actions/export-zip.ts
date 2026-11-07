/**
 * 上游 ZIP 导出路由的桌面壳载体（SH-114 批 2）。
 *
 * 上游 `SessionLogDownloadController` 的形状是「`HEAD` 探活 → `save()` 触发下载」：
 * `fetcher` 默认打 `session.export?…` 的 HTTP 路由，`save` 默认造一个 `<a download>`。
 * 桌面壳两样都没有，所以这里注入同形替身：
 *
 * - **`HEAD` 只做真探活**：调 `export_probe`（档案能不能读），拿回会话头。
 *   会话不存在/档案损坏 → 404，controller 进 error 态并把原因显示出来。
 * - **`save` 触发真实导出**：保存对话框（`pick_save_file`）→ `load_session` 全量档案
 *   → 上游那份事件翻译 → 调 `export_session_log` 由 Rust 压成 ZIP。
 *
 * **为什么 save 不 await、失败也不回头改 Dialog 状态**：上游的 `save` 是
 * `void` 返回的 fire-and-forget，它把 success 定义成"下载已开始"（浏览器下载管理器
 * 接手之后失败也不在 Dialog 里显示）。桌面壳照这个语义分层——「保存对话框取消」是
 * 用户主动行为，静默结束；「写盘失败」是下载开始之后的事，走日志留痕。
 * 想让失败可见需要另立 toast 通道，**不要**在这里 hack vendored controller 的内部 store。
 *
 * **为什么 canonical JSONL 由前端产出、ZIP 由 Rust 编码**：见
 * `.workbuddy/reports/shaniu-m4-sh114-batch2-plan-2026-11-07.md` D1——事件语义只有
 * `session-events.ts` 一份实现（与 Markdown 导出同源），Rust 只负责压缩与路径安全，
 * 避免出现"导出 / 回放 / 搜索"三套事件推导。
 */
import type { SessionEvent } from '@deepseek-ai/dsh-session/types'
import { SESSION_LOG_EXPORT_ROUTE } from '../dsh/packages/session-query/session-log-export/src/routes'
import type { HostBridge } from '../transport/tauri'
import { sessionEntriesToEvents, type SessionEntryLike } from './session-events'

/**
 * DSH 当前逻辑格式版本与 canonical 日志文件名。
 *
 * **为什么不从 vendored 源码 import**：上游这份值在
 * `src/dsh/packages/core/session/src/types.ts:90`（`SESSION_FORMAT_VERSION = 4`），
 * 文件名规则在 `session-format/src/filename.ts`（0 → `session.jsonl`，N → `session.vN.jsonl`）。
 * 但 `session-format/src/types.ts` 在 SHANIU 的编译选项下会报 TS2411（它的索引签名
 * 与可选字段不兼容），一旦按值 import 就把整包类型错误拖进主程序。
 *
 * **同步义务**：上游升版时这里与 `export-zip.spec.ts` 的守门断言一起红，提醒同步；
 * 不要改成"看起来更优雅"的深层 import——那是已经踩过的坑（批 1 的类型闭包盲区）。
 */
export const SESSION_FORMAT_VERSION = 4

/**
 * 当前代的 canonical 日志 basename（与上游 `sessionFormatLogFilename` 同规则）。
 *
 * @param version - 逻辑格式版本，默认当前版本。
 * @returns 版本 0 为 `session.jsonl`，其后为 `session.vN.jsonl`。
 */
export function sessionLogFilename(version: number = SESSION_FORMAT_VERSION): string {
  return version === 0 ? 'session.jsonl' : `session.v${String(version)}.jsonl`
}

/** 与上游 `type Fetch` 同形；只用到 `ok` / `status` / `text()`。 */
type FetchLike = (input: string | URL, init?: RequestInit) => Promise<Response>

/** 与上游 `type Save` 同形：fire-and-forget，controller 不 await。 */
type SaveLike = (url: string, filename: string) => void

/**
 * `export_probe` 的返回（Rust `session_export::ExportHeader` 的 serde 形状）。
 *
 * 字段是 snake_case：Tauri 只转换**顶层命令参数名**，结构体内部字段仍按 serde 的
 * 原样（与 `save_providers` 的 `providers[]` 同一条规则，见 docs/05 SH-105 补修注）。
 */
export interface ExportHeaderLike {
  readonly id: string
  readonly created_at: number
  readonly cwd: string | null
  readonly parent_session: string | null
  readonly delegation_depth: number
}

/** 宿主 `export_session_log` 的返回。 */
interface ExportReportLike {
  readonly path: string
  readonly bytes: number
  readonly entries: number
}

/** 路由命中失败时给调用方的可读原因（上游按 `HTTP ${status}` 拼提示）。 */
function failure(status: number, detail = ''): Response {
  return {
    ok: status >= 200 && status < 300,
    status,
    text: async () => detail,
  } as unknown as Response
}

/**
 * 把上游的 Markdown 路由换成宿主探活。
 *
 * 只接管 ZIP 那一条；别的路由返回 404 而不是静默成功——批 1 的 Markdown 载体
 * 已经接管自己那条，两条互不重叠，"点了没反应"才不会被静默吞掉。
 *
 * @param bridge - 宿主命令桥。
 * @returns 供 `SessionLogDownloadController` 构造注入的 fetcher。
 */
export function exportZipFetcher(bridge: HostBridge): FetchLike {
  return async (input, init) => {
    const url = new URL(String(input), 'http://shaniu.local/')
    if (url.pathname.replace(/^\//u, '') !== SESSION_LOG_EXPORT_ROUTE) {
      return failure(404)
    }
    const sessionId = url.searchParams.get('sessionId') ?? ''
    if (sessionId === '') return failure(404)
    init?.signal?.throwIfAborted()
    try {
      const header = await bridge.invoke<ExportHeaderLike>('export_probe', { sessionId })
      init?.signal?.throwIfAborted()
      return failure(200, JSON.stringify(header))
    } catch (reason: unknown) {
      // 会话不存在 / 档案中间坏行 / 读盘失败：都让 controller 进 error 态并带上原因。
      return failure(404, reason instanceof Error ? reason.message : String(reason))
    }
  }
}

/**
 * 按上游 `serializeSessionLog` 的规则拼 canonical JSONL。
 *
 * 与上游唯一的差别是 header 来源：上游从持久化句柄拿 `SessionHeader`，这里从
 * `export_probe` 拿同一个语义的翻译结果。字段顺序与可选性逐条对齐上游
 * `archive.ts:110-136`（`cwd`/`parentSession` 为空则省略，`isSeeded` 必填，
 * `delegationDepth` 缺省 0）——顺序影响逐字对比，不要"顺手重排"。
 *
 * @param header - 探活返回的会话头。
 * @param events - 事件序列（`sessionEntriesToEvents` 产出）。
 * @returns header 行 + 每事件一行 + 单一尾换行。
 */
export function serializeSessionLog(
  header: ExportHeaderLike,
  events: readonly SessionEvent[],
): string {
  const lines = [JSON.stringify({
    type: 'session',
    version: SESSION_FORMAT_VERSION,
    id: header.id,
    createdAt: header.created_at,
    ...header.cwd === null ? {} : { cwd: header.cwd },
    ...header.parent_session === null ? {} : { parentSession: header.parent_session },
    isSeeded: false,
    delegationDepth: header.delegation_depth ?? 0,
  })]
  for (const event of events) lines.push(JSON.stringify(event))
  return `${lines.join('\n')}\n`
}

/**
 * 造一个"保存对话框 + 写盘"的 `save` 实现。
 *
 * 弹出对话框后重新探活一次：用户可能在对话框上停留很久，期间会话被写或被删，
 * 用第一帧的旧头会导出一份"头与正文不一致"的归档。
 *
 * @param bridge - 宿主命令桥。
 * @param titleOf - 会话标题取值器（与 Markdown 导出同源，冷会话回落 id）。
 * @param warn - 失败留痕（写盘失败发生在 Dialog 已报 success 之后）。
 * @returns 供 `SessionLogDownloadController` 构造注入的 save。
 */
export function exportZipSaver(
  bridge: HostBridge,
  titleOf: (sessionId: string) => string,
  warn: (message: string, error: unknown) => void,
): SaveLike {
  return (input, filename) => {
    void (async () => {
      const url = new URL(String(input), 'http://shaniu.local/')
      const sessionId = url.searchParams.get('sessionId') ?? ''
      if (sessionId === '') throw new Error('导出请求缺少 sessionId')

      // 用户取消 = 主动行为，不是错误：直接结束，不写盘、不提示。
      const dest = await bridge.invoke<string | null>('pick_save_file', { defaultName: filename })
      if (dest === null) return

      const header = await bridge.invoke<ExportHeaderLike>('export_probe', { sessionId })
      const entries = await bridge.invoke<SessionEntryLike[]>('load_session', { agentId: sessionId })
      const events = sessionEntriesToEvents(entries, titleOf(sessionId))

      // 子会话（subagents/<id>/…，待 SH-056）与附件（media/…、files/…）两段的
      // 数据面都还没接线；这里留出组装点而不是写死"没有这段"——接线时只往
      // entries 里 push，不改命令面。
      const zipEntries: Array<{ path: string; content: string }> = [{
        path: sessionLogFilename(),
        content: serializeSessionLog(header, events),
      }]

      await bridge.invoke<ExportReportLike>('export_session_log', { dest, entries: zipEntries })
    })().catch((error: unknown) => {
      warn('会话日志导出失败', error)
    })
  }
}
