/**
 * SHANIU 自有工作区行菜单插件（宿主侧，非 vendored）：补齐工作区菜单的中间项。
 *
 * 上游工作区菜单三项：重命名(100) / 在文件夹显示(200) / 删除工作区(300)。中间项
 * 来自 ui-open-in-app 包（SHANIU 未整体移植——它还带着会话/文件/URL 等一大套
 * 原生打开面）；这里只补工作区这一项，直接走会话头部资源管理器按钮在用的
 * `open_in_explorer` 宿主命令。默认工作区和其他工作区都生效。
 *
 * 走上游既有接缝（contextMenus 注册表 + contextMenu.icon 槽位），不改任何移植源码。
 */
import { IconFolderOpenOutlineRegular } from '@deepseek-ai/dsh-client-ui-primitives'
import { isDarwinDesktop } from '@deepseek-ai/dsh-client-ui-primitives'
import type { Context } from '@deepseek-ai/cordis'
import type {} from '@deepseek-ai/dsh-client-ui-context-menu/client'
// 类型侧引入：'workspace' 目标族由 ui-workspace 的 contract/slots.ts 扩充进目标表。
import type {} from '@deepseek-ai/dsh-client-ui-workspace/client'
import { inTauri, openInSystemExplorer } from '../header-actions/open-workspace'
import { en, NS, zh } from './locales'

/** 必需服务：槽位、词典、菜单。 */
export const inject = ['slots', 'locale', 'contextMenus']

/**
 * 注册工作区行的「在文件夹显示」菜单贡献。
 * @param ctx - 客户端根上下文。
 */
export function apply(ctx: Context): void {
  ctx.effect(() => ctx.locale.register(NS, { zh, en }), 'shaniu-workspace-menu: dictionaries')
  const t = ctx.locale.bind(NS)

  // 上游以「安装应用目录里有 finder」取词；SHANIU 没有应用清单，按平台判：
  // macOS 桌面必有 Finder，其余平台取资源管理器口径——与上游在这两类
  // 环境下的实际取词一致。
  const revealLabel = (): string => t(isDarwinDesktop() ? 'menu.finder' : 'menu.reveal')

  ctx.slots.inject('contextMenu.icon', function* () {
    // 键名沿用上游 ui-open-in-app 的图标席位，未来整体移植时可以无缝替换。
    yield ctx.slots.register({ name: 'contextMenu.icon', key: 'open-in-app.reveal' }, IconFolderOpenOutlineRegular)
  })

  ctx.effect(() => ctx.contextMenus.register({
    id: 'shaniu.workspace.reveal',
    target: 'workspace',
    order: 200,
    resolve: ({ path }) => [{
      id: 'reveal',
      label: revealLabel(),
      icon: { key: 'open-in-app.reveal' },
      // 浏览器 dev 没有系统文件管理器可调，如实置灰而不是点了没反应。
      disabled: !inTauri(),
      failureLabel: t('menu.revealFailed'),
      run: async () => {
        if (!await openInSystemExplorer(path)) throw new Error(t('menu.revealFailed'))
      },
    }],
  }), 'shaniu-workspace-menu: reveal workspace in file manager')
}
