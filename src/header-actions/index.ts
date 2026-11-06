/**
 * SHANIU 自有会话头部插件（宿主侧，非 vendored）：补齐标题右侧的两个入口。
 *
 * - 资源管理器：占用 `conversation.session.header.utilities`（与官方 ui-open-in-app 同槽同序同命令位），
 *   点击在系统文件管理器里打开当前会话的工作区目录。
 * - 更多操作：ui-conversation 的头部已经内建了 `contextMenu.button`（label「更多操作」），
 *   但只有 `sessionHeader` 目标存在菜单贡献时才渲染；这里补贡献，按钮随即出现。
 *
 * 两处都走上游既有接缝，不改任何移植源码。
 */
import type { Context } from '@deepseek-ai/cordis'
import type { SessionId } from '@deepseek-ai/dsh-session/types'
import type {} from '@deepseek-ai/dsh-client-ui-renderer/client'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import type {} from '@deepseek-ai/dsh-client-ui-context-menu/client'
import { ExplorerButton, OPEN_LOCAL_COMMAND } from './ExplorerButton'
import { openInSystemExplorer } from './open-workspace'
import { sessionToMarkdown } from './session-markdown'
import { en, NS, zh } from './locales'

/** 必需服务：槽位、词典、菜单、快捷键、会话与对话视图。 */
export const inject = ['slots', 'locale', 'contextMenus', 'shortcuts', 'sessions', 'uiConversation']

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
  const t = ctx.locale.bind(NS)

  // 直接打开当前会话的工作区目录；浏览器 dev 环境没有这个能力，保持静默
  const openExplorer = (sessionId: SessionId): void => {
    const cwd = ctx.sessions.list.getSnapshot().byId[sessionId]?.cwd
    if (cwd === undefined) return
    void openInSystemExplorer(cwd).catch((error: unknown) => {
      ctx.logger.warn('打开文件管理器失败: %s', error)
    })
  }

  // 与官方一致的绑定：桌面 Ctrl+Alt+O，Web 让位给 workspace.add 改用 Ctrl+Shift+O
  ctx.effect(() => ctx.shortcuts.register({
    id: OPEN_LOCAL_COMMAND,
    label: () => t('explorer.tooltip', { app: t('explorer.app') }),
    aliases: ['open workspace locally', 'open in app'],
    defaults: {
      'desktop:macos': { code: 'KeyO', modifiers: ['primary', 'alt'] },
      'desktop:windows': { code: 'KeyO', modifiers: ['primary', 'alt'] },
      'desktop:linux': { code: 'KeyO', modifiers: ['primary', 'alt'] },
      'web:macos': { code: 'KeyO', modifiers: ['primary', 'shift'] },
      'web:windows': { code: 'KeyO', modifiers: ['primary', 'shift'] },
    },
    regions: ['page', 'editable'],
    modals: [],
    resolve: () => {
      // 快捷键没有按钮携带的会话，取当前挂在主视图上的那一条（与官方同一判据）
      const row = Object.values(ctx.sessions.list.getSnapshot().byId).find(entry => (entry.retainedBy.mainView ?? 0) > 0)
      if (row === undefined) return { status: 'blocked', reason: t('explorer.noSession') }
      return { status: 'handled', run: () => { openExplorer(row.id) } }
    },
  }), 'shaniu-header: open explorer shortcut')

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

  // Markdown 取自 Chat 视图已经落定的节点，与界面所见同源
  const sessionMarkdown = (sessionId: SessionId): string => {
    const title = ctx.sessions.list.getSnapshot().byId[sessionId]?.displayTitle ?? sessionId
    const snapshot = ctx.uiConversation.binding(sessionId).target('chat').getSnapshot()
    return sessionToMarkdown(title, snapshot === undefined ? [] : snapshot.legacy.nodes)
  }

  ctx.effect(() => ctx.contextMenus.register({
    id: 'shaniu.session-header',
    target: 'sessionHeader',
    order: 0,
    resolve: ({ sessionId }) => [
      {
        id: 'copy-markdown',
        label: t('menu.copyMarkdown'),
        icon: { key: 'workspace.copy' },
        failureLabel: t('menu.copyFailed'),
        run: async () => { await navigator.clipboard.writeText(sessionMarkdown(sessionId)) },
      },
      {
        id: 'copy-id',
        label: t('menu.copyId'),
        icon: { key: 'workspace.copy' },
        separatorBefore: true,
        failureLabel: t('menu.copyFailed'),
        run: async () => { await navigator.clipboard.writeText(sessionId) },
      },
    ],
  }), 'shaniu-header: more menu')
}
