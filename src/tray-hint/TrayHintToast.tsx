import { useEffect, useState } from 'react'
import { listen } from '@tauri-apps/api/event'
import { IconWarningOutlineRegular, Toast } from '@deepseek-ai/dsh-client-ui-primitives'

/** 与宿主 `user_terminal::TRAY_HINT_EVENT` 同名。 */
export const TRAY_HINT_EVENT = 'user-terminal://tray-hint'

/** 两行文案要读完，默认 3 秒偏短。 */
const HOLD_MS = 6000

interface Hint {
  /** 每次提示递增，用作 Toast 的 key：新的提示会重新开始一次生命周期。 */
  readonly seq: number
  readonly count: number
}

/**
 * 隐藏到托盘时的一次性提示。
 * @returns 有提示时渲染 toast，否则不占位。
 */
export function TrayHintToast() {
  const [hint, setHint] = useState<Hint | null>(null)

  useEffect(() => {
    let disposed = false
    let unlisten: (() => void) | undefined
    void listen<{ count: number }>(TRAY_HINT_EVENT, event => {
      const count = event.payload.count
      if (count <= 0) return
      setHint(previous => ({ seq: (previous?.seq ?? 0) + 1, count }))
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
  return (
    <Toast
      key={shown.seq}
      text={`仍有 ${String(shown.count)} 个终端会话在运行，退出应用才会终止。`}
      icon={<IconWarningOutlineRegular />}
      holdMs={HOLD_MS}
      onDone={() => {
        // 只在当前这条提示结束时清空；期间来了新提示就保留新的。
        setHint(current => current?.seq === shown.seq ? null : current)
      }}
    />
  )
}
