import type { ReactNode } from 'react'
import type { ShortcutCatalogEntry, ShortcutCommandId } from '@deepseek-ai/dsh-client-shortcuts/client'
import type { HostObservable, InjectFace, PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import { IconFolderOpenRegular, Tooltip } from '@deepseek-ai/dsh-client-ui-primitives'
import type {} from './locales'
import './explorer-button.css'

/** 与官方同名的命令位：Ctrl+Alt+O 打开工作区。 */
export const OPEN_LOCAL_COMMAND = 'workspace.openLocal' as ShortcutCommandId

type ExplorerButtonProps =
  & PropsRuntime<'conversation.session.header.utilities'>
  & PropsLocale<'shaniuHeader'>
  & InjectFace<{
    open: () => void
    hooks: { shortcuts: HostObservable<readonly ShortcutCatalogEntry[]> }
  }>

/**
 * 标题行右侧的资源管理器按钮。
 *
 * 图标、提示与快捷键对齐官方 `ui-open-in-app`：标准文件夹图标、`用 {app} 打开`、
 * `workspace.openLocal`；外形保留它的紧凑形态（24px 高 / 11px / 0.5px 描边），
 * 但**去掉 chevron 半区**（有意偏离，已登记 `docs/07`）：上游那半个是真实下拉入口，
 * 本项目只有一个打开目标，画上去会诱导无效点击。
 */
export function ExplorerButton({ open, t, useShortcuts }: ExplorerButtonProps): ReactNode {
  const shortcut = useShortcuts(rows => rows.find(row => row.id === OPEN_LOCAL_COMMAND))
  const label = t('explorer.tooltip', { app: t('explorer.app') })
  return (
    <Tooltip label={label} shortcutKeys={shortcut?.keys} side="bottom" align="end" portal>
      <button
        type="button"
        className="explorer-button"
        aria-label={label}
        onClick={() => { open() }}
      >
        <IconFolderOpenRegular size={13} />
      </button>
    </Tooltip>
  )
}
