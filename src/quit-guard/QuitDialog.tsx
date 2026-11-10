import { useEffect, useState } from 'react'
import { listen } from '@tauri-apps/api/event'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import { QUIT_DIALOG_EVENT, QUIT_REQUEST_EVENT, type QuitSurvey } from './quit'
import type { NS } from './locales'

/** 由插件注入的能力：会话标题解析与三个动作。 */
export interface QuitDialogInjected {
  /** 会话 id → 用户可读标题（取侧栏同一份列表快照）。 */
  readonly titleOf: (sessionId: string) => string
  /** 先取消任务、限时等待、再退出。 */
  readonly cancelAndQuit: () => Promise<void>
  /** 收回托盘（窗口隐藏，进程继续）。 */
  readonly hideToTray: () => Promise<void>
}

interface Body {
  readonly survey: QuitSurvey
  readonly phase: 'idle' | 'cancelling' | 'error'
  readonly error?: string
}

const BACKDROP: React.CSSProperties = {
  position: 'fixed', inset: 0, zIndex: 60,
  background: 'rgba(0, 0, 0, 0.42)',
  display: 'flex', alignItems: 'center', justifyContent: 'center',
}

const CARD: React.CSSProperties = {
  width: 'min(460px, calc(100vw - 48px))',
  boxSizing: 'border-box',
  padding: '20px 22px 18px',
  borderRadius: 12,
  background: 'var(--dsw-alias-bg-layer-1)',
  color: 'var(--dsw-alias-label-primary)',
  boxShadow: '0 18px 48px rgba(0, 0, 0, 0.28)',
  font: '14px/1.6 system-ui, "Segoe UI", sans-serif',
}

const LIST: React.CSSProperties = { margin: '10px 0 0', padding: 0, listStyle: 'none' }
const ROW: React.CSSProperties = { color: 'var(--dsw-alias-label-secondary)', marginTop: 4 }
const ACTIONS: React.CSSProperties = {
  display: 'flex', justifyContent: 'flex-end', gap: 8, marginTop: 18, flexWrap: 'wrap',
}
const BUTTON: React.CSSProperties = {
  padding: '7px 14px', borderRadius: 8, border: '1px solid var(--dsw-alias-bg-layer-3)',
  background: 'transparent', color: 'inherit', cursor: 'pointer', font: 'inherit',
}
const PRIMARY: React.CSSProperties = {
  ...BUTTON,
  borderColor: 'transparent',
  background: '#cf222e',
  color: '#fff',
}

/**
 * 真退出前的“还有任务”对话框。
 *
 * 三个选项都必须给路：取消任务并退出 / 继续跑 / 收回托盘。没有任务时宿主不会
 * 到这里（`requestQuit` 直接退出），所以这里永远只处理“确实还有事情在跑”。
 */
export function QuitDialog({
  t, titleOf, cancelAndQuit, hideToTray,
}: PropsLocale<typeof NS> & QuitDialogInjected) {
  const [body, setBody] = useState<Body | null>(null)

  useEffect(() => {
    let disposed = false
    const unlisteners: (() => void)[] = []
    void listen<QuitSurvey>(QUIT_REQUEST_EVENT, event => {
      if (!event.payload.busy) return
      setBody({ survey: event.payload, phase: 'idle' })
    }).then(stop => { if (disposed) stop(); else unlisteners.push(stop) })

    const onLocal = (event: Event): void => {
      const survey = (event as CustomEvent<QuitSurvey>).detail
      if (survey?.busy === true) setBody({ survey, phase: 'idle' })
    }
    window.addEventListener(QUIT_DIALOG_EVENT, onLocal)
    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') setBody(null)
    }
    window.addEventListener('keydown', onKey)
    return () => {
      disposed = true
      for (const stop of unlisteners) stop()
      window.removeEventListener(QUIT_DIALOG_EVENT, onLocal)
      window.removeEventListener('keydown', onKey)
    }
  }, [])

  if (body === null) return null
  const { survey, phase, error } = body
  const busy = phase === 'cancelling'
  const jobSessions = [...new Set(
    survey.runningJobs
      .map(job => job.sessionId)
      .filter((id): id is string => id !== undefined),
  )].map(titleOf)

  const onCancelAndQuit = (): void => {
    setBody(current => current === null ? null : { ...current, phase: 'cancelling', error: undefined })
    void cancelAndQuit().catch((reason: unknown) => {
      setBody(current => current === null ? null : {
        ...current,
        phase: 'error',
        error: reason instanceof Error ? reason.message : String(reason),
      })
    })
  }

  return (
    <div style={BACKDROP} role="presentation">
      <div style={CARD} role="dialog" aria-modal="true" aria-labelledby="shaniu-quit-title">
        <h2 id="shaniu-quit-title" style={{ margin: 0, fontSize: 16, lineHeight: 1.4 }}>
          {t('quit.title')}
        </h2>
        <p style={{ margin: '8px 0 0', color: 'var(--dsw-alias-label-secondary)' }}>{t('quit.body')}</p>
        <ul style={LIST}>
          {survey.activeSession !== undefined && (
            <li style={ROW}>{t('quit.activeTask', { title: titleOf(survey.activeSession) })}</li>
          )}
          {survey.runningJobs.length > 0 && (
            <li style={ROW}>{t('quit.jobs', {
              count: survey.runningJobs.length,
              sessions: jobSessions.length > 0 ? jobSessions.join('、') : '—',
            })}</li>
          )}
          {survey.runningTerminals > 0 && (
            <li style={ROW}>{t('quit.terminals', { count: survey.runningTerminals })}</li>
          )}
          {survey.pendingApprovals > 0 && (
            <li style={ROW}>{t('quit.approvals', { count: survey.pendingApprovals })}</li>
          )}
        </ul>
        {phase === 'cancelling' && (
          <p style={{ ...ROW, marginTop: 12 }} role="status">{t('quit.cancelling')}</p>
        )}
        {phase === 'error' && (
          <p style={{ ...ROW, marginTop: 12, color: '#cf222e' }} role="alert">
            {t('quit.failed', { message: error ?? '' })}
          </p>
        )}
        <div style={ACTIONS}>
          <button
            type="button"
            style={BUTTON}
            disabled={busy}
            onClick={() => { setBody(null); void hideToTray() }}
          >
            {t('quit.hideToTray')}
          </button>
          <button type="button" style={BUTTON} disabled={busy} onClick={() => { setBody(null) }}>
            {t('quit.keepRunning')}
          </button>
          <button type="button" style={PRIMARY} disabled={busy} onClick={onCancelAndQuit}>
            {t('quit.cancelAndQuit')}
          </button>
        </div>
      </div>
    </div>
  )
}
