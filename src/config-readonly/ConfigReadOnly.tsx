/**
 * 配置只读的共享提示（宿主侧）。
 *
 * 启动时 `config.toml` 解析失败 → 应用按只读运行（fail-closed，原文件不动）。
 * 这是**全局状态**，不是某一功能专有：MCP 连接器页与通用设置页挂的是同一个
 * 组件、同一条词典键 `configReadOnly`，两处不会出现两套措辞。
 */
import { useEffect, useState, type ReactNode } from 'react'
import { tauriBridge } from '../transport/tauri'
import './config-readonly.css'

/**
 * 读取配置只读原因（`get_settings.configReadOnly`）。
 * 该状态在启动时确定、重读配置要重启应用，所以只读一次。
 * @returns 非空字符串 = 解析失败及原因（含出错行与配置文件路径）；null = 配置正常。
 */
export function useConfigReadOnly(): string | null {
  const [reason, setReason] = useState<string | null>(null)
  useEffect(() => {
    let live = true
    tauriBridge()
      .invoke<{ configReadOnly?: string | null }>('get_settings')
      .then(
        (settings) => { if (live) setReason(settings.configReadOnly ?? null) },
        // 读不到设置不是本提示的错误：不把「配置只读」误报出来。
        () => { if (live) setReason(null) },
      )
    return () => { live = false }
  }, [])
  return reason
}

/**
 * 只读横幅本体。
 *
 * 刻意**不用** `role="alert"`：配置只读是启动时确定的静态状态，每次进设置页都被
 * 读屏打断是副作用；用普通段落 + 12px 警示色，进入页面即可读到，不抢焦点。
 * @param props.message - 已按词典键 `configReadOnly` 生成的整句（文案只有一份）。
 * @returns 横幅元素。
 */
export function ConfigReadOnlyNotice({ message }: { readonly message: string }): ReactNode {
  return <p className="config-readonly-notice">{message}</p>
}

/** 通用设置页里的一行：有只读原因才出现，整行就是横幅。 */
export function GeneralConfigReadOnlyRow({ t }: {
  readonly t: (key: 'configReadOnly', params: { reason: string }) => string
}): ReactNode {
  const reason = useConfigReadOnly()
  if (reason === null) return null
  return (
    <div className="config-readonly-row">
      <ConfigReadOnlyNotice message={t('configReadOnly', { reason })} />
    </div>
  )
}
