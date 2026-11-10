/**
 * SHANIU 自有会话头部插件（宿主侧，非 vendored）：补齐标题右侧的两个入口。
 *
 * - 资源管理器：占用 `conversation.session.header.utilities`（与官方 ui-open-in-app 同槽同序同命令位），
 *   点击在系统文件管理器里打开当前会话的工作区目录。
 * - 更多操作：ui-conversation 的头部已经内建了 `contextMenu.button`（label「更多操作」），
 *   但只有 `sessionHeader` 目标存在菜单贡献时才渲染；这里补贡献，按钮随即出现。
 *
 * **SH-114 批 1**：「复制为 Markdown」不再用本地临时实现（`session-markdown.ts` 已退役），
 * 改为挂上游 `session-log-export` 的 `SessionMarkdownCopyController`——同一份
 * `formatSessionMarkdown`、同一套 CopyToast 与并发规则。上游那条路依赖 host 的
 * `session.markdown` HTTP 路由，桌面壳没有 HTTP 面，所以用**构造注入的 fetcher**
 * 换成 `load_session` + 本地渲染（见 `export-markdown.ts`），不改任何移植源码。
 *
 * **SH-114 批 2**：「下载会话日志」同理挂上游 `SessionLogDownloadController` +
 * `SessionLogDownloadDialog`——HEAD 探活换成 `export_probe`，`save` 换成
 * 「保存对话框 + Rust 写 ZIP」（见 `export-zip.ts`）。菜单因此是三入口：
 * 复制为 Markdown / 下载会话日志 / 复制会话 ID。
 *
 * **`/export` 命令（SH-115）**：与菜单入口共享同一个 `SessionLogDownloadController`
 * 实例——命令面（adapter 的 `remote.commands.execute`）经 `transport/command-triggers`
 * 的触发总线调到这里，零第二条导出链。菜单没装配（插件未启动）时命令如实报错。
 */
import type { Context } from '@deepseek-ai/cordis'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import type {} from '@deepseek-ai/dsh-client-ui-context-menu/client'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import { IconDownloadOutlineRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import { SessionMarkdownCopyController } from '../dsh/packages/session-query/session-log-export/src/client/copy-controller'
import {
  SessionLogDownloadController,
} from '../dsh/packages/session-query/session-log-export/src/client/controller'
import {
  SessionLogDownloadDialog, type SessionLogDownloadDialogInjected,
} from '../dsh/packages/session-query/session-log-export/src/client/Dialog'
import {
  SessionMarkdownCopyToast, type SessionMarkdownCopyToastInjected,
} from '../dsh/packages/session-query/session-log-export/src/client/CopyToast'
import {
  en as exportEn, NS as EXPORT_NS, zh as exportZh, type SessionLogDownloadKey,
} from '../dsh/packages/session-query/session-log-export/src/client/locales'
import { tauriBridge } from '../transport/tauri'
import { registerExportTrigger } from '../transport/command-triggers'
import { writeClipboardText } from '../host/clipboard'
import { ExplorerButton } from './ExplorerButton'
import { openInSystemExplorer } from './open-workspace'
import { exportMarkdownFetcher } from './export-markdown'
import { exportZipFetcher, exportZipSaver } from './export-zip'
import { en, NS, zh } from './locales'

/** 必需服务：槽位、词典、菜单、快捷键、会话与对话视图。 */
export const inject = ['slots', 'locale', 'contextMenus', 'shortcuts', 'sessions', 'uiConversation']

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 导出反馈（复制 toast / 下载对话框）的词典命名空间，由上游 `session-log-export` 定义。 */
    'session-log-download': SessionLogDownloadKey
  }
}

declare module '@deepseek-ai/dsh-client-ui-context-menu/client' {
  interface ContextMenuTargetMap {
    /** 标题行右侧的「更多操作」；ui-conversation 的头部工厂按钮以此为目标。 */
    sessionHeader: { readonly sessionId: SessionId }
  }
}

/**
 * 注册资源管理器按钮、快捷键与「更多操作」菜单贡献。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-header: dictionaries')
  ctx.effect(() => ctx.locale.register(EXPORT_NS, { zh: exportZh, en: exportEn }), 'shaniu-header: export dictionaries')
  const t = ctx.locale.bind(NS)

  // 直接打开当前会话的工作区目录。失败一律发声：这条链曾经在"列表行没有 cwd"时
  // 静默 return，产物回归里表现为「点按钮毫无反应、console 也没有任何输出」，
  // 排查代价远高于一条 warn（与 cordis FAILED fiber 是同一个病）。
  const openExplorer = (sessionId: SessionId): void => {
    const list = ctx.sessions.list.getSnapshot()
    const row = list.byId[sessionId]
    const cwd = row?.cwd
    if (cwd === undefined) {
      ctx.logger.warn(
        '资源管理器：会话 %s 的列表行%s（列表 phase=%s，共 %d 条），没有工作区目录可打开',
        sessionId,
        row === undefined ? '不存在' : '缺 cwd',
        list.phase,
        list.ids.length,
      )
      return
    }
    void openInSystemExplorer(cwd).then((opened) => {
      // 浏览器 dev 直开页面没有 Tauri 内部对象：这是预期的"做不到"，但也要留痕。
      if (!opened) {
        ctx.logger.warn('资源管理器：当前页面不是 Tauri 宿主，%s 没有执行', cwd)
      }
    }).catch((error: unknown) => {
      ctx.logger.warn('资源管理器：打开 %s 失败: %s', cwd, error)
    })
  }

  // 快捷键复用 vendored `ui-open-in-app` 的 `workspace.openLocal`（桌面 Ctrl+Alt+O）：
  // 同 id 二次注册会被 ShortcutsRegistry 判为重复并抛出，整颗插件 fiber 因此回滚
  // （见 dsh-boot 的装载失败扫描）。按钮只读 catalog 里的那一行拿键位提示。

  ctx.slots.inject('conversation.session.header.utilities', () => ctx.slots.register({
    name: 'conversation.session.header.utilities',
    id: 'shaniu-explorer',
    order: -10,
    locale: NS,
    inject: (sessionId: SessionId) => ({
      hooks: { shortcuts: ctx.shortcuts.catalog },
      open: () => { openExplorer(sessionId) },
    }),
  }, ExplorerButton))

  // 会话标题取自列表快照（与侧栏同一来源）；冷会话也有条目，取不到才回落 id。
  const titleOf = (sessionId: string): string =>
    ctx.sessions.list.getSnapshot().byId[sessionId as SessionId]?.displayTitle ?? sessionId

  const copyController = new SessionMarkdownCopyController(
    exportMarkdownFetcher(tauriBridge(), titleOf),
    {
      // 桌面壳不用 deferred write：文本要经 `load_session` 异步取，而
      // `ClipboardItem` 的延迟写依赖点击激活窗口，长导出会越过激活期；宿主
      // `write_clipboard_text` 是同步落系统剪贴板的，更稳且与粘贴读取同源。
      supportsDeferredWrite: () => false,
      writeDeferred: async () => {},
      writeText: async (text: string) => {
        try {
          await writeClipboardText(text)
          return true
        } catch {
          return false
        }
      },
    },
  )
  ctx.effect(() => async () => { await copyController.dispose() }, 'shaniu-header: markdown copy lifecycle')

  // SH-114 批 2：ZIP 导出。上游 controller 的「HEAD 探活 → save 触发」形状原样保留，
  // 只把 HTTP 载体换成宿主命令（探活 = export_probe；保存 = 对话框 + 写盘），
  // 取舍与"为什么 save 是 fire-and-forget"见 export-zip.ts 文件头。
  const downloadController = new SessionLogDownloadController(
    exportZipFetcher(tauriBridge()),
    exportZipSaver(tauriBridge(), titleOf, (message, error) => {
      ctx.logger.warn('%s: %s', message, error)
    }),
  )
  ctx.effect(() => async () => { await downloadController.dispose() }, 'shaniu-header: zip download lifecycle')

  // SH-115：/export 命令共用这条下载链（触发总线见 transport/command-triggers.ts）。
  // 插件卸载时注销，命令面在无执行者时报"导出面不可用"而不是触达已 dispose 的控制器。
  ctx.effect(() => registerExportTrigger((sessionId) => { void downloadController.download(sessionId) }),
    'shaniu-header: /export trigger wiring')

  // 菜单图标：上游把这组 key 注册在自己的 client 插件里，SHANIU 没挂它的 apply，
  // 所以在这里补同一个 key——菜单项只认 key，图标实现放哪不影响文案与行为。
  ctx.slots.inject('contextMenu.icon', function* () {
    yield ctx.slots.register({
      name: 'contextMenu.icon',
      key: 'session-log-download.download',
    }, IconDownloadOutlineRegular)
  })

  ctx.slots.inject('shell.overlay', () => ctx.slots.register({
    name: 'shell.overlay',
    id: 'shaniu-session-copy-toast',
    locale: EXPORT_NS,
    inject: (): SessionMarkdownCopyToastInjected => ({
      hooks: { sessionMarkdownCopy: copyController.store },
      dismiss: () => { copyController.dismiss() },
    }),
  }, SessionMarkdownCopyToast))

  ctx.slots.inject('shell.overlay', () => ctx.slots.register({
    name: 'shell.overlay',
    id: 'shaniu-session-log-download',
    locale: EXPORT_NS,
    inject: (): SessionLogDownloadDialogInjected => ({
      hooks: { sessionLogDownload: downloadController.store },
      dismiss: (sessionId: SessionId) => { downloadController.dismiss(sessionId) },
    }),
  }, SessionLogDownloadDialog))

  const tExport = ctx.locale.bind(EXPORT_NS)

  ctx.effect(() => ctx.contextMenus.register({
    id: 'shaniu.session-header',
    target: 'sessionHeader',
    order: 0,
    // 下载项在飞行中要变禁用、复制项在复制中要变禁用——上游靠 observe 让菜单
    // 在 store 变化时重算；少了它只有"下一次打开菜单"才看得到状态，点了第二次
    // 才发现被合并（controller 会把并发请求并进同一次操作，不至于出错，但表现为
    // "点了没反应"）。
    observe: () => [copyController.store, downloadController.store, ctx.locale],
    resolve: ({ sessionId }) => [
      {
        id: 'copy-markdown',
        label: t('menu.copyMarkdown'),
        icon: { key: 'workspace.copy' },
        disabled: copyController.store.getSnapshot().copying,
        failureLabel: t('menu.copyFailed'),
        // controller 把失败转成 toast 通知，不在这里抛；与上游菜单同一用法。
        run: () => copyController.copy(sessionId),
      },
      {
        id: 'download',
        // 文案直接取 vendored 词典（8 种语言现成），不往本插件词典再抄一份。
        label: tExport('menu.download'),
        icon: { key: 'session-log-download.download' },
        disabled: downloadController.store.getSnapshot().bySession[sessionId]?.status === 'downloading',
        failureLabel: t('menu.copyFailed'),
        run: () => downloadController.download(sessionId),
      },
      {
        id: 'copy-id',
        label: t('menu.copyId'),
        icon: { key: 'workspace.copy' },
        separatorBefore: true,
        failureLabel: t('menu.copyFailed'),
        run: async () => { await writeClipboardText(sessionId) },
      },
    ],
  }), 'shaniu-header: more menu')
}
