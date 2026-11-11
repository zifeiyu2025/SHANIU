/**
 * 通用设置页的「默认目录」一行（宿主插件，不是 vendored 组件）。
 *
 * 为什么需要这一行：`data\Workspace` 曾经在每次启动时无条件创建；现在只在
 * **这次真的以默认根工作**时才建（`run()` 里按 `AppState::workspace_root()` 判定），
 * 避免在数据根里留一个永远不会被使用的空目录。行为变了，界面必须说清楚，
 * 否则用户会以为"默认目录没了"或"默认目录就是正在用的目录"。
 *
 * 文案口径由代码事实决定，不沿用任何"当前未使用"的笼统说法：
 *  - 未配置（或被工作区判据拒绝）时，默认根 = `<数据目录>\Workspace`，并且在用；
 *  - 配置了合法自定义根时，界面显示的是配置值，默认目录不被使用、也不会创建；
 *  - 两种情况下"只在实际使用它时创建"都成立。
 */
import { useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import { tauriBridge } from '../transport/tauri'
import { NS } from './locales'
import './default-dir.css'

/** `get_product_dirs` 里本行用到的字段。 */
interface ProductDirs {
  readonly workspaceRoot?: string
}

/** `get_settings` 里本行用到的字段（生效的工作区根）。 */
interface SettingsView {
  readonly workspaceRoot?: string
}

/** 默认根与生效根。两者都可能相同（正在使用默认目录）。 */
export interface DefaultDirState {
  readonly defaultRoot: string
  readonly activeRoot: string
}

/**
 * 路径比较口径：Windows 大小写不敏感，分隔符与尾部分隔符写法不唯一。
 * 只用于"是不是同一个目录"的展示判断，不参与任何安全判定。
 */
export function samePath(left: string, right: string): boolean {
  const normalize = (value: string): string => value.trim().replace(/\//g, '\\').replace(/\\+$/, '').toLowerCase()
  return normalize(left) === normalize(right)
}

/**
 * 读取两个路径：默认根（`get_product_dirs`）与生效根（`get_settings`）。
 * 任一缺失或调用失败都返回 `null`——宁可不显示，也不猜一个路径挂在界面上。
 */
export function useDefaultDir(): DefaultDirState | null {
  const [state, setState] = useState<DefaultDirState | null>(null)
  useEffect(() => {
    let live = true
    const bridge = tauriBridge()
    void Promise.all([
      bridge.invoke<ProductDirs>('get_product_dirs'),
      bridge.invoke<SettingsView>('get_settings'),
    ]).then(
      ([dirs, settings]) => {
        if (!live) return
        const defaultRoot = dirs.workspaceRoot?.trim()
        const activeRoot = settings.workspaceRoot?.trim()
        if (defaultRoot === undefined || defaultRoot === '' || activeRoot === undefined || activeRoot === '') {
          setState(null)
          return
        }
        setState({ defaultRoot, activeRoot })
      },
      // 读不到不是本行的错误：静默不显示，不写成"没有默认目录"。
      () => { if (live) setState(null) },
    )
    return () => { live = false }
  }, [])
  return state
}

/**
 * 一行只读信息：默认目录是什么、什么时候会被创建、当前工作区是不是它。
 * @param props.t - 词典绑定（`shaniuDefaultDir`）。
 * @returns 行元素；路径读不到时不渲染。
 */
export function DefaultDirRow({ t }: PropsLocale<typeof NS>): ReactNode {
  const state = useDefaultDir()
  if (state === null) return null
  // 生效根与默认根不同时才补一句：此时界面上的工作区不是这个默认目录。
  const usingDefault = samePath(state.defaultRoot, state.activeRoot)
  return (
    <div className="default-dir-row">
      <span className="default-dir-title">{t('title')}</span>
      <code className="default-dir-path">{state.defaultRoot}</code>
      <p className="default-dir-hint">{t('hint')}</p>
      {usingDefault ? null : <p className="default-dir-hint">{t('active', { path: state.activeRoot })}</p>}
    </div>
  )
}
