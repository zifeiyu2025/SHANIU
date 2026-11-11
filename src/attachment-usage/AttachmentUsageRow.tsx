/**
 * 通用设置页的「附件占用」一行（宿主插件，不是 vendored 组件）。
 *
 * 只读：调用宿主 `attachment_stats` 拿 `attachments/` 的总文件数与总字节数。
 * 读取失败显示「—」而不是 0——0 会被误读成「没有附件」。
 */
import { useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import { tauriBridge } from '../transport/tauri'
import { NS } from './locales'
import './attachment-usage.css'

/** `attachment_stats` 的返回形状。 */
export interface AttachmentUsage {
  readonly fileCount: number
  readonly totalBytes: number
}

/**
 * 人类可读的字节数：B 不带小数，其余保留一位。
 * @param bytes - 字节数（负数按 0 处理，宿主不会给出负数）。
 * @returns 形如 `1.5 KB`。
 */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${Math.max(0, bytes)} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(1)} ${units[unit]}`
}

/**
 * 读取附件占用。
 * @returns 统计值；读取失败保持 `null`（界面显示「—」）。
 */
export function useAttachmentUsage(): AttachmentUsage | null {
  const [usage, setUsage] = useState<AttachmentUsage | null>(null)
  useEffect(() => {
    let live = true
    void tauriBridge().invoke<AttachmentUsage>('attachment_stats').then(
      (value) => { if (live) setUsage(value) },
      // 读不到不是本行的错误，但也不能当成 0：保持 null，界面显示「—」。
      () => { if (live) setUsage(null) },
    )
    return () => { live = false }
  }, [])
  return usage
}

/**
 * 一行只读信息：附件目录里有多少文件、占多少字节。
 * @param props.t - 词典绑定（`shaniuAttachmentUsage`）。
 * @returns 行元素。
 */
export function AttachmentUsageRow({ t }: PropsLocale<typeof NS>): ReactNode {
  const usage = useAttachmentUsage()
  return (
    <div className="attachment-usage-row">
      <span className="attachment-usage-title">{t('title')}</span>
      <code className="attachment-usage-value">
        {usage === null
          ? '—'
          : t('value', { count: usage.fileCount, size: formatBytes(usage.totalBytes) })}
      </code>
      <p className="attachment-usage-hint">{t('hint')}</p>
    </div>
  )
}
