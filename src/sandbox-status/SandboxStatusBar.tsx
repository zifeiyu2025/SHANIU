/**
 * SH-021 沙箱提示：`shell.bottom` 的**按需**占用者。
 *
 * 对齐 DSH 的前提（2026-11-11 定）：同一档位只允许有一套词、一个落点。
 * 档位由输入区的访问模式 chip 展示（DSH `PermissionSelect`），这里不再重复；
 * 常态整条不渲染，只在下面四种情况出现：
 *  1. `enforcement = partial`：逻辑沙箱，文件限制没有全部强制；
 *  2. 状态读取失败：看不见状态本身就是异常，不能静默；
 *  3. 文件权限正在后台准备（`ancestor = preparing`）：刚启动时个别命令可能失败；
 *  4. 文件权限没准备好（`ancestor = needs_admin`）：只在真出错时才有影响。
 *
 * 3/4 只给**一行字**：没有进度条、没有弹窗、不跑百分比。
 * `needs_admin` **不允许拦人**——不禁用输入、不弹窗、不要求立刻提权；
 * 真的执行失败时，解释由宿主在错误信息里给（`sandbox::ancestor_hint`），
 * 这里只把事实摆出来。
 *
 * 文案纪律：这里**不得**出现「只读 / 工作区可写 / 完全放开」这类自造档位词，
 * 也不得出现实现细节（目录层级 / 安全描述符 / 授权对象）；档位措辞统一交给 chip。
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

/** 准备中的轮询间隔：状态只在这一段时间里会变，落到终态就停表。 */
const PREPARING_POLL_MS = 1000

/**
 * 渲染沙箱提示；常态返回 `null`。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 宿主命令实现；缺省走 Tauri。
 * @returns 提示条或空。
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

  const partial = loadError === null && status?.enforcement === 'partial'
  const preparing = loadError === null && status?.ancestor === 'preparing'
  const needsAdmin = loadError === null && status?.ancestor === 'needs_admin'

  // 准备期间每秒拉一次，到终态（ready / needs_admin）就停：
  // 不新增事件通道，轮询是这里最省的做法，且只有一个状态需要盯。
  useEffect(() => {
    if (!preparing) return
    const timer = setInterval(() => { void refresh() }, PREPARING_POLL_MS)
    return () => { clearInterval(timer) }
  }, [preparing, refresh])

  // 常态不显示：档位归输入区 chip，`idle` / `ready` 没有任何需要用户处理的信息。
  if (loadError === null && !partial && !preparing && !needsAdmin) return null

  const tone = loadError !== null || partial || needsAdmin ? (loadError !== null ? 'error' : 'warn') : 'info'
  const text = loadError !== null
    ? t('error.status', { error: loadError })
    : needsAdmin
      ? t('notice.needs_admin')
      : partial
        ? t('notice.partial')
        : t('notice.preparing')
  // 准备中会自查，用户没有可点的动作；读取失败与逻辑沙箱保留手动刷新。
  const showRefresh = loadError !== null || partial

  return (
    <div
      className="shaniu-sandbox-notice"
      data-tone={tone}
      role="status"
      aria-live="polite"
    >
      {tone === 'info' ? null : (
        <IconWarningTriangleOutlineRegular className="shaniu-sandbox-notice-icon" />
      )}
      <span className="shaniu-sandbox-notice-text">{text}</span>
      {showRefresh ? (
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
      ) : null}
    </div>
  )
}
