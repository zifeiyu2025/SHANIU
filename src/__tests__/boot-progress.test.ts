import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createBootProgress } from '../host/boot-progress'

// 这些用例验的是启动页状态机的时序契约（docs/01 §9.5）：
// 延迟出现、最短可见、交接淡出、失败出口。用假时钟同时驱动 setTimeout 与
// performance.now，才能断言"在某一毫秒时页面是什么样"。

function mount(): { splash: HTMLElement; status: HTMLElement } {
  document.body.innerHTML = '<div id="boot-splash"><div data-boot-status></div></div>'
  const splash = document.getElementById('boot-splash')
  const status = splash?.querySelector<HTMLElement>('[data-boot-status]') ?? null
  if (splash === null || status === null) throw new Error('fixture 构建失败')
  return { splash, status }
}

beforeEach(() => {
  vi.useFakeTimers({
    toFake: ['setTimeout', 'clearTimeout', 'setInterval', 'clearInterval', 'Date', 'performance'],
  })
})

afterEach(() => {
  vi.useRealTimers()
  document.body.innerHTML = ''
})

describe('createBootProgress', () => {
  it('启动快于阈值时不显示阶段文案，直接淡出并移除启动页', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status)
    boot.report('准备工作区')
    await vi.advanceTimersByTimeAsync(100)
    // 100ms < 250ms：还没到揭示时机
    expect(status.classList.contains('is-visible')).toBe(false)
    expect(status.textContent).toBe('')

    const settled = boot.done()
    await vi.advanceTimersByTimeAsync(160)
    await settled
    expect(splash.classList.contains('is-leaving')).toBe(true)
    expect(document.getElementById('boot-splash')).toBeNull()
  })

  it('超过阈值才揭示，揭示后阶段文案保持最新值', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status)
    boot.report('准备工作区')
    await vi.advanceTimersByTimeAsync(249)
    expect(status.classList.contains('is-visible')).toBe(false)

    await vi.advanceTimersByTimeAsync(1)
    expect(status.textContent).toBe('准备工作区')
    expect(status.classList.contains('is-visible')).toBe(true)

    await vi.advanceTimersByTimeAsync(300)
    boot.report('装载界面插件 7/46')
    expect(status.textContent).toBe('装载界面插件 7/46')
  })

  it('阶段切换受最短可见约束：太快到达的下一段等上一段停够再上', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status)
    boot.report('准备工作区')
    await vi.advanceTimersByTimeAsync(250)
    expect(status.textContent).toBe('准备工作区')

    // 只过了 100ms 就来了新阶段：不立刻切换
    await vi.advanceTimersByTimeAsync(100)
    boot.report('渲染界面')
    expect(status.textContent).toBe('准备工作区')

    // 上一段满 300ms 后补上新值
    await vi.advanceTimersByTimeAsync(199)
    expect(status.textContent).toBe('准备工作区')
    await vi.advanceTimersByTimeAsync(1)
    expect(status.textContent).toBe('渲染界面')
  })

  it('done() 会等最短可见走完再淡出，避免启动页一闪而过', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status)
    boot.report('准备工作区')
    await vi.advanceTimersByTimeAsync(250)
    expect(status.classList.contains('is-visible')).toBe(true)

    // 揭示后 50ms 就绪：还要再显示 250ms
    await vi.advanceTimersByTimeAsync(50)
    let settled = false
    const settling = boot.done().then(() => {
      settled = true
    })
    await vi.advanceTimersByTimeAsync(249)
    expect(settled).toBe(false)
    expect(splash.classList.contains('is-leaving')).toBe(false)
    // 就绪后即交出输入：等待最短可见期间不能再挡住应用
    expect(splash.style.pointerEvents).toBe('none')

    await vi.advanceTimersByTimeAsync(1)
    expect(splash.classList.contains('is-leaving')).toBe(true)
    await vi.advanceTimersByTimeAsync(160)
    await settling
    expect(splash.isConnected).toBe(false)
  })

  it('超时切到可诊断面板：说明停在哪个阶段、日志在哪、提供重试', async () => {
    const { splash, status } = mount()
    const onRetry = vi.fn()
    const boot = createBootProgress(splash, status, { onRetry })
    boot.report('装载界面插件 12/46')

    await vi.advanceTimersByTimeAsync(7999)
    expect(splash.querySelector('.boot-failure')).toBeNull()

    await vi.advanceTimersByTimeAsync(1)
    const panel = splash.querySelector('.boot-failure')
    expect(panel).not.toBeNull()
    expect(panel?.textContent).toContain('启动没有完成')
    expect(panel?.textContent).toContain('超过 8 秒')
    expect(panel?.textContent).toContain('停在：装载界面插件 12/46')
    expect(panel?.textContent).toContain('日志：')

    const retry = splash.querySelector<HTMLButtonElement>('.boot-retry')
    expect(retry).not.toBeNull()
    retry?.click()
    expect(onRetry).toHaveBeenCalledTimes(1)
  })

  it('启动失败时保留失败原因，且不再被后续阶段或超时覆盖', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status, { logHint: 'C:\\logs' })
    boot.fail({ message: '连接宿主超时' })
    expect(splash.textContent).toContain('连接宿主超时')
    expect(splash.textContent).toContain('日志：C:\\logs')

    boot.report('渲染界面')
    await vi.advanceTimersByTimeAsync(9000)
    expect(splash.textContent).toContain('连接宿主超时')
    expect(splash.querySelector('.boot-status')).toBeNull()
  })

  it('超时面板亮起后应用才就绪：done() 撤掉面板，不留挡住可用界面的错误态', async () => {
    const { splash, status } = mount()
    const boot = createBootProgress(splash, status)
    await vi.advanceTimersByTimeAsync(8000)
    expect(splash.querySelector('.boot-failure')).not.toBeNull()

    await boot.done()
    expect(splash.isConnected).toBe(false)
  })
})
