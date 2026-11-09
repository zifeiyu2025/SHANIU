/**
 * SH-021 沙箱异常提示：`shell.bottom` 的**按需**占用者。
 *
 * 对齐 DSH 的前提（2026-11-11 定）：同一档位只允许有一套词、一个落点。
 * 档位由输入区的访问模式 chip 展示（DSH `PermissionSelect`），这里不再重复；
 * 常态（强制完整）整条不渲染，只在两种异常下出现：
 *  1. `enforcement = partial`：逻辑沙箱，文件限制没有全部强制；
 *  2. 状态读取失败：看不见状态本身就是异常，不能静默。
 *
 * 文案纪律：这里**不得**出现「只读 / 工作区可写 / 完全放开」这类自造档位词，
 * 档位措辞统一交给 chip 的词典（仅可查看 / 工作区内修改 / 完全权限）。
 *
 * **不显示网络**（2026-11-10 定，对齐 DSH）：网络不是策略维度，容器一律可出网。
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import {
  Button,
  IconRefreshOutlineRegular,
  IconWarningTriangleOutlineRegular,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { NS } from './locales'
import { hostSandboxWire, type SandboxStatusWire, type SandboxWire } from './wire'
import './sandbox-status.css'

/** 桌面运行时的默认数据面（模块级常量：保持引用稳定，避免 effect 每次重跑）。 */
const DEFAULT_WIRE = hostSandboxWire()

/** 提示条拿到的 props：根部标准套件 + 本命名空间词典 + 可注入的数据面（测试用）。 */
type SandboxStatusBarProps = PropsRuntime<'shell.bottom'> & PropsLocale<typeof NS> & {
  readonly wire?: SandboxWire
}

/**
 * 渲染沙箱异常提示；常态返回 `null`。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 宿主命令实现；缺省走 Tauri。
 * @returns 异常提示条或空。
 */
export function SandboxStatusBar({ t, wire = DEFAULT_WIRE }: SandboxStatusBarProps): ReactNode {
  const [status, setStatus] = useState<SandboxStatusWire | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setStatus(await wire.status())
      setLoadError(null)
    } catch (error: unknown) {
      setLoadError(String(error))
    }
  }, [wire])

  useEffect(() => { void refresh() }, [refresh])

  // 常态不显示：档位归输入区 chip，强制完整时没有任何需要用户处理的信息。
  if (loadError === null && (status === null || status.enforcement === 'full')) return null

  const partial = loadError === null && status?.enforcement === 'partial'

  return (
    <div
      className="shaniu-sandbox-notice"
      data-tone={partial ? 'warn' : 'error'}
      role="status"
      aria-live="polite"
    >
      <IconWarningTriangleOutlineRegular className="shaniu-sandbox-notice-icon" />
      <span className="shaniu-sandbox-notice-text">
        {partial ? t('notice.partial') : t('error.status', { error: loadError ?? '' })}
      </span>
      <Button
        className="shaniu-sandbox-notice-refresh"
        size="sm"
        variant="ghost"
        aria-label={t('refresh')}
        title={t('refresh')}
        onClick={() => void refresh()}
      >
        <IconRefreshOutlineRegular />
      </Button>
    </div>
  )
}
