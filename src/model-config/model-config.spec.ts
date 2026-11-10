import { describe, expect, it, vi } from 'vitest'
import { apply } from './index'

interface Registration {
  options: Record<string, unknown>
  component: unknown
}

interface Provider {
  id: string
  name: string
}

interface SeatInject {
  open: () => void
  loading: boolean
  failed: boolean
}

/** 记录注册与事件的最小 ctx：提供商目录可按用例翻转。 */
function harness(initial: readonly Provider[] | null = [], options: { fail?: boolean } = {}) {
  const registrations: Registration[] = []
  const disposed: string[] = []
  const listeners = new Map<string, (() => void)[]>()
  const warn = vi.fn()
  let providers = initial
  let fail = options.fail === true
  const openSection = vi.fn()

  const ctx = {
    effect: (fn: () => unknown) => fn(),
    locale: { register: () => () => {}, bind: () => (key: string) => key },
    logger: { warn },
    settingsShell: { openSection },
    slots: {
      inject: (_name: string, callback: () => () => void) => {
        callback()
        return () => {}
      },
      register: (options: Record<string, unknown>, component: unknown) => {
        registrations.push({ options, component })
        return () => { disposed.push(String(options.id ?? options.name)) }
      },
    },
    remote: {
      ...(initial === null ? {} : {
        llm: {
          listProviders: async () => {
            if (fail) throw new Error('宿主命令不可达')
            return { ok: true as const, value: providers ?? [] }
          },
        },
      }),
      $on: (event: string, listener: () => void) => {
        listeners.set(event, [...(listeners.get(event) ?? []), listener])
        return () => {}
      },
    },
  }
  apply(ctx as never)
  return {
    registrations,
    disposed,
    openSection,
    warn,
    setProviders(next: readonly Provider[]) { providers = next },
    setFail(next: boolean) { fail = next },
    emit(event: string) {
      for (const listener of listeners.get(event) ?? []) listener()
    },
  }
}

/** 等一轮微任务：sync 里的 listProviders 是异步的。 */
const flush = async (): Promise<void> => {
  await Promise.resolve()
  await Promise.resolve()
}

/** 取最后一次注册的席位注入面。 */
function seatInjects(registrations: Registration[]): SeatInject[] {
  return registrations
    .filter(item => item.options.name === 'conversation.input.model')
    .map(item => (item.options.inject as () => SeatInject)())
}

describe('model config seat', () => {
  it('出厂没有任何提供商时接管席位：先加载态，再空态，点击打开设置-模型', async () => {
    const { registrations, openSection } = harness([])
    await flush()

    const seats = registrations.filter(item => item.options.name === 'conversation.input.model')
    // 首帧 loading → 确认零提供商后 empty（重新注册，注入面随形态变）
    expect(seats).toHaveLength(2)
    expect(seatInjects(registrations).map(item => item.loading)).toEqual([true, false])
    for (const seat of seats) expect(seat.options.priority).toBe(-1)

    const latest = seatInjects(registrations).at(-1)
    latest?.open()
    expect(openSection).toHaveBeenCalledWith('models')
  })

  it('配置好提供商后撤回影子，且不再反复重注册', async () => {
    const { registrations, disposed, setProviders, emit } = harness([])
    await flush()
    expect(registrations).toHaveLength(2)

    setProviders([{ id: 'mock-provider', name: 'Mock Provider' }])
    emit('credentials/reference-updated')
    await flush()
    expect(disposed).toEqual(['conversation.input.model', 'conversation.input.model'])
    expect(registrations).toHaveLength(2)

    setProviders([])
    emit('llm/adapters-updated')
    await flush()
    expect(registrations).toHaveLength(3)
    expect(seatInjects(registrations).at(-1)?.loading).toBe(false)
  })

  it('远端面缺失时最终交还上游，不留影子', async () => {
    const { registrations, disposed } = harness(null)
    await flush()
    // 首帧 loading 注册一次，确认远端面不可用后立即撤回
    expect(registrations).toHaveLength(1)
    expect(disposed).toHaveLength(1)
  })

  it('读取提供商 reject 时不卡在 loading：转入失败态并留一条可定位日志', async () => {
    const { registrations, warn, openSection, setFail, emit } = harness([], { fail: true })
    await flush()

    // 反向对照：旧实现里 sync 的 reject 没人接，席位会停在唯一那次 loading 注册。
    const seats = seatInjects(registrations)
    expect(seats.map(seat => seat.loading)).toEqual([true, false])
    expect(seats.at(-1)?.failed).toBe(true)

    // 日志要能定位到失败的调用（命令名 + 原因），不是一句「出错了」。
    expect(warn).toHaveBeenCalledTimes(1)
    expect(String(warn.mock.calls[0]?.[0])).toContain('listProviders')
    expect(String(warn.mock.calls[0]?.[1])).toContain('宿主命令不可达')

    // 失败态仍可点开设置：不是一块死掉的占位。
    seats.at(-1)?.open()
    expect(openSection).toHaveBeenCalledWith('models')

    // 宿主恢复后，事件驱动重试能离开失败态（失败态不是终点）。
    setFail(false)
    emit('llm/models-updated')
    await flush()
    expect(seatInjects(registrations).at(-1)).toMatchObject({ loading: false, failed: false })
  })
})
