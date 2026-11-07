/**
 * 上游 Markdown 路由的桌面壳载体（SH-114 批 1）。
 *
 * 上游 `SessionMarkdownCopyController` 通过 `fetch('session.markdown?sessionId=…')`
 * 向宿主要 Markdown 文本——那是 Web 形态的 HTTP 路由。桌面壳没有 HTTP server，
 * 所以这里注入一个**同形状的 `Fetch`**：把那条相对路径映射到 `load_session` 命令，
 * 再用上游 `formatSessionMarkdown` 渲染。
 *
 * **为什么不改 vendored 源码**：`AGENTS.md` 对 `src/dsh/**` 只放行 React 19 兼容修复。
 * 上游把 `fetcher` 设计成构造参数，正是为了让宿主换载体——用它提供的接缝，而不是改它。
 *
 * **为什么 Markdown 由前端渲染、不由 Rust 生成**：`formatSessionMarkdown` 是纯函数，
 * 上游只有这一份实现。若在 Rust 重写一份，就出现"两个 formatter"，输出细节
 * （折叠阈值、fence 补闭、省略清单）迟早分叉；让宿主只负责"给全量条目"、格式化仍走
 * 上游那份，才谈得上"与上游口径逐条一致"。
 */
import type { HostBridge } from '../transport/tauri'
// 从具体文件取，不走包入口：包入口（`src/index.ts`）是 host 插件，会连带引入
// `archive.ts`（ZIP 条目/fflate/附件收集），批 1 用不到，不该为一次复制把它拖进首屏。
import { formatSessionMarkdown } from '../dsh/packages/session-query/session-log-export/src/markdown'
import { SESSION_MARKDOWN_EXPORT_ROUTE } from '../dsh/packages/session-query/session-log-export/src/routes'
import { sessionEntriesToEvents, type SessionEntryLike } from './session-events'

/** 与上游 `type Fetch` 同形；只用到 `ok` / `status` / `text()`。 */
type FetchLike = (input: string | URL, init?: RequestInit) => Promise<Response>

/** 路由命中失败时给调用方的可读原因（上游按 `HTTP ${status}` 拼提示）。 */
function notFound(): Response {
  return {
    ok: false,
    status: 404,
    text: async () => '',
  } as unknown as Response
}

/**
 * 造一个只认 Markdown 路由的 `Fetch`。
 *
 * @param bridge - 宿主命令桥（Tauri invoke）。
 * @param titleOf - 会话标题取值器；上游 formatter 用它做一级标题。
 * @returns 供 `SessionMarkdownCopyController` 构造注入的 fetcher。
 */
export function exportMarkdownFetcher(
  bridge: HostBridge,
  titleOf: (sessionId: string) => string,
): FetchLike {
  return async (input, init) => {
    // 上游拼的是 document-relative 路径（`session.markdown?...`），
    // 这里用固定 base 解析出 pathname 与 query，避免依赖当前 location。
    const url = new URL(String(input), 'http://shaniu.local/')
    if (`${url.pathname.replace(/^\//u, '')}` !== SESSION_MARKDOWN_EXPORT_ROUTE) {
      // 只接管 Markdown 这一条。别的路由（ZIP 下载，批 2）不该被这里静默吞掉，
      // 否则"点了没反应"会变成最难查的一类问题。
      return notFound()
    }
    const sessionId = url.searchParams.get('sessionId') ?? ''
    if (sessionId === '') return notFound()
    init?.signal?.throwIfAborted()
    // `load_session` 读的是整份档案，不依赖界面已加载多少——这正是本卡
    // 「未加载历史也能导出」的落点。
    const entries = await bridge.invoke<SessionEntryLike[]>('load_session', { agentId: sessionId })
    init?.signal?.throwIfAborted()
    const markdown = formatSessionMarkdown(sessionEntriesToEvents(entries, titleOf(sessionId)))
    return {
      ok: true,
      status: 200,
      text: async () => markdown,
    } as unknown as Response
  }
}
