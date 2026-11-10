import { useEffect, useState } from 'react'
import { listen } from '@tauri-apps/api/event'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import { IconWarningOutlineRegular, Toast } from '@deepseek-ai/dsh-client-ui-primitives'
import type { NS } from './locales'

/** 与宿主 `user_terminal::TRAY_HINT_EVENT` 同名。 */
export const TRAY_HINT_EVENT = 'user-terminal://tray-hint'

/** 两行文案要读完，默认 3 秒偏短。 */
const HOLD_MS = 8000

interface Hint {
  /** 每次提示递增，用作 Toast 的 key：新的提示会重新开始一次生命周期。 */
  readonly seq: number
  readonly count: number
  readonly first: boolean
}

/**
 * 隐藏到托盘时的一次性提示。
 *
 * 关闭窗口不是退出：每次关闭都必须明确告诉用户“程序还在、怎么真正退出”。
 * 有任务在跑时带上数量（更醒目），首次关闭再补一句托盘图标位置。
 * @param props.t - 本命名空间词典（中英两语）。
 * @returns 有提示时渲染 toast，否则不占位。
 */
export function TrayHintToast({ t }: PropsLocale<typeof NS>) {
  const [hint, setHint] = useState<Hint | null>(null)

  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined
    void listen<{ count: number; first?: boolean }>(TRAY_HINT_EVENT, event => {
      setHint(previous => ({
        seq: (previous?.seq ?? 0) + 1,
        count: event.payload.count,
        first: event.payload.first === true,
      }))
    }).then(stop => {
      if (disposed) stop()
      else unlisten = stop
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [])

  if (hint === null) return null
  const shown = hint
  const base = shown.count > 0
    ? t('trayHintRunning', { count: shown.count })
    : t('trayHintIdle')
  const text = shown.first ? `${base} ${t('trayHintFirst')}` : base
  return (
    <Toast
      key={shown.seq}
      text={text}
      icon={<IconWarningOutlineRegular />}
      holdMs={HOLD_MS}
      onDone={() => {
        // 只在当前这条提示结束时清空；期间来了新提示就保留新的。
        setHint(current => current?.seq === shown.seq ? null : current)
      }}
    />
  )
}
