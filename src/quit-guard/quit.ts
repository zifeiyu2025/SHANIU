/**
 * 退出流程的数据面：盘点 → 无任务直接退 / 有任务弹三个选项。
 *
 * 关闭窗口只收托盘，真正退出必须显式触发。托盘菜单与「更多操作」共用这一条链：
 * 两者都先 `quit_survey`，避免出现“菜单知道有任务、托盘不知道”的两套判断。
 */
import { tauriBridge } from '../transport/tauri'

/** 与宿主 `lib.rs::QUIT_REQUEST_EVENT` 同名（托盘 → 前端）。 */
export const QUIT_REQUEST_EVENT = 'app://quit-requested'

/** 前端内部事件：菜单触发时把盘点结果交给对话框。 */
export const QUIT_DIALOG_EVENT = 'shaniu://quit-requested'

/** 与宿主 `QuitSurvey` 同形。 */
export interface QuitJob {
  readonly id: string
  readonly sessionId?: string
  readonly label: string
  readonly status: string
}

export interface QuitSurvey {
  readonly activeSession?: string
  readonly runningJobs: readonly QuitJob[]
  readonly runningTerminals: number
  readonly pendingApprovals: number
  readonly busy: boolean
}

/**
 * 请求退出：没有在跑的事就直接退，有就弹三选项对话框。
 *
 * 供「更多操作 → 退出 SHANIU」调用；托盘菜单走宿主事件，最终落到同一个对话框。
 */
export async function requestQuit(): Promise<void> {
  const bridge = tauriBridge()
  const survey = await bridge.invoke<QuitSurvey>('quit_survey')
  if (!survey.busy) {
    await bridge.invoke('quit_app')
    return
  }
  window.dispatchEvent(new CustomEvent<QuitSurvey>(QUIT_DIALOG_EVENT, { detail: survey }))
}
