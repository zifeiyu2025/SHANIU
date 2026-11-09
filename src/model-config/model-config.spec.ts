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
}

/** 记录注册与事件的最小 ctx：提供商目录可按用例翻转。 */
function harness(initial: readonly Provider[] | null = []) {
  const registrations: Registration[] = []
  const disposed: string[] = []
  const listeners = new Map<string, (() => void)[]>()
  let providers = initial
  const openSection = vi.fn()

  const ctx = {
    effect: (fn: () => unknown) => fn(),
    locale: { register: () => () => {}, bind: () => (key: string) => key },
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
          listProviders: async () => ({ ok: true as const, value: providers ?? [] }),
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
    setProviders(next: readonly Provider[]) { providers = next },
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
})
