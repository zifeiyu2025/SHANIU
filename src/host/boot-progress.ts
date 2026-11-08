/**
 * 启动页状态机（L1 静态启动页 → L2 真实阶段反馈 → L3 交接淡出 / 可诊断失败态）。
 *
 * 设计约束（docs/01 §9.5）：
 * - 不造假进度：只显示真实阶段文案（有真实比例的才带 X/N），不画进度条，动效只是呼吸脉冲。
 * - 延迟出现 + 最短可见：不到 250ms 就完成的启动不显示阶段文案（一闪而过比不显示更难看）；
 *   一旦出现，至少停留 300ms，避免阶段文案抽搐。
 * - 交接无闪：启动页与应用首屏同底色（--boot-bg），React 挂载后由本模块淡出，不存在
 *   "启动页撤了、应用还没画上"的中间帧。
 * - 失败有出口：超时或异常切成可诊断面板（停在哪一步、日志位置、重试），不是永久等待。
 * - 尊重 prefers-reduced-motion：动画与过渡在 CSS 里关掉，逻辑层不额外补动效。
 */

/** 失败态需要的最小事实：出了什么事，停在哪一步。 */
export interface BootFailure {
  message: string
  stage?: string | null
}

export interface BootProgressOptions {
  /** 超过这个时长才显示阶段文案，默认 250ms。 */
  revealAfterMs?: number
  /** 阶段文案一旦出现至少停留这么久，默认 300ms。 */
  minVisibleMs?: number
  /** 交接淡出时长，需与 index.html 的 transition 一致，默认 160ms。 */
  fadeMs?: number
  /** 等待就绪的上限，超过则切到可诊断状态，默认 8000ms。 */
  timeoutMs?: number
  /** 失败面板里展示的日志位置。 */
  logHint?: string
  /** 重试动作，默认重新加载页面。 */
  onRetry?: () => void
}

export interface BootProgress {
  /** 上报一个真实阶段。可高频调用，本模块负责去抖与最短可见。 */
  report: (label: string) => void
  /** 界面已就绪：等最短可见时长走完后淡出并移除启动页。 */
  done: () => Promise<void>
  /** 启动失败：切成可诊断面板，保持可见。 */
  fail: (failure: BootFailure) => void
}

const DEFAULT_LOG_HINT = '%LOCALAPPDATA%\\SHANIU\\logs（便携版为程序目录下 data\\logs）'

function delay(ms: number): Promise<void> {
  return new Promise(resolve => {
    setTimeout(resolve, ms)
  })
}

/**
 * 在给定节点上运行启动页状态机。
 * @param splash 启动页根节点（L1），本模块负责淡出与移除。
 * @param status 阶段文案节点（L2）。
 */
export function createBootProgress(
  splash: HTMLElement,
  status: HTMLElement,
  options: BootProgressOptions = {},
): BootProgress {
  const revealAfter = options.revealAfterMs ?? 250
  const minVisible = options.minVisibleMs ?? 300
  const fadeMs = options.fadeMs ?? 160
  const timeoutMs = options.timeoutMs ?? 8000
  const logHint = options.logHint ?? DEFAULT_LOG_HINT

  const startedAt = performance.now()
  let latest: string | null = null
  let shownLabel: string | null = null
  let shownAt = 0
  let revealed = false
  let settled = false
  let failed = false
  let revealTimer: ReturnType<typeof setTimeout> | undefined
  let applyTimer: ReturnType<typeof setTimeout> | undefined
  let timeoutTimer: ReturnType<typeof setTimeout> | undefined

  const cancel = (timer: ReturnType<typeof setTimeout> | undefined): void => {
    if (timer !== undefined) clearTimeout(timer)
  }

  function paint(label: string): void {
    if (shownLabel !== label) {
      status.textContent = label
      shownLabel = label
    }
    status.classList.add('is-visible')
    shownAt = performance.now()
  }

  function applyLatest(): void {
    if (latest === null) return
    if (!revealed) {
      revealed = true
      paint(latest)
      return
    }
    const remaining = minVisible - (performance.now() - shownAt)
    if (remaining <= 0) {
      paint(latest)
      return
    }
    // 上一段还没停够：到点再补一次，用最新值，避免中间态一闪而过
    if (applyTimer === undefined) {
      applyTimer = setTimeout(() => {
        applyTimer = undefined
        applyLatest()
      }, remaining)
    }
  }

  function report(label: string): void {
    if (settled) return
    latest = label
    if (revealed) {
      applyLatest()
      return
    }
    const elapsed = performance.now() - startedAt
    if (elapsed >= revealAfter) {
      applyLatest()
      return
    }
    if (revealTimer === undefined) {
      revealTimer = setTimeout(() => {
        revealTimer = undefined
        applyLatest()
      }, revealAfter - elapsed)
    }
  }

  async function done(): Promise<void> {
    if (failed) {
      // 超时面板已经亮过，但应用后来又起来了：撤掉面板，不留一个挡住可用界面的错误态。
      settled = true
      splash.remove()
      return
    }
    if (settled) return
    settled = true
    cancel(revealTimer)
    cancel(applyTimer)
    cancel(timeoutTimer)
    // 就绪之后启动页只是视觉层：即使下面还要等最短可见时长，也不能再挡住应用的操作。
    splash.style.pointerEvents = 'none'
    if (revealed) {
      const remaining = minVisible - (performance.now() - shownAt)
      if (remaining > 0) await delay(remaining)
    }
    splash.classList.add('is-leaving')
    await delay(fadeMs)
    splash.remove()
  }

  function fail(failure: BootFailure): void {
    if (settled) return
    settled = true
    failed = true
    cancel(revealTimer)
    cancel(applyTimer)
    cancel(timeoutTimer)
    splash.classList.remove('is-leaving')

    const wrap = document.createElement('div')
    wrap.className = 'boot-failure'
    const title = document.createElement('div')
    title.className = 'boot-failure-title'
    title.textContent = '启动没有完成'
    const message = document.createElement('div')
    message.textContent = failure.message
    const stage = document.createElement('div')
    stage.className = 'boot-failure-stage'
    stage.textContent = `停在：${failure.stage ?? latest ?? '未知阶段'}`
    const hint = document.createElement('code')
    hint.textContent = `日志：${logHint}`
    const retry = document.createElement('button')
    retry.type = 'button'
    retry.className = 'boot-retry'
    retry.textContent = '重试'
    retry.addEventListener('click', () => {
      if (options.onRetry !== undefined) options.onRetry()
      else window.location.reload()
    })
    wrap.append(title, message, stage, hint, retry)
    splash.replaceChildren(wrap)
  }

  timeoutTimer = setTimeout(() => {
    fail({ message: `等待界面就绪超过 ${Math.round(timeoutMs / 1000)} 秒` })
  }, timeoutMs)

  return {
    report,
    done: async () => {
      cancel(timeoutTimer)
      await done()
    },
    fail,
  }
}
