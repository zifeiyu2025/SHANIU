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

describe('model config seat', () => {
  it('出厂没有任何提供商时接管席位，点击打开设置-模型', async () => {
    const { registrations, openSection } = harness([])
    await flush()
    const seat = registrations.find(item => item.options.name === 'conversation.input.model')
    expect(seat?.options.priority).toBe(-1)
    const injected = (seat?.options.inject as () => { open: () => void })()
    injected.open()
    expect(openSection).toHaveBeenCalledWith('models')
  })

  it('配置好提供商后撤回影子，再次清空时重新接管', async () => {
    const { registrations, disposed, setProviders, emit } = harness([])
    await flush()
    expect(registrations).toHaveLength(1)

    setProviders([{ id: 'mock-provider', name: 'Mock Provider' }])
    emit('credentials/reference-updated')
    await flush()
    expect(disposed).toContain('conversation.input.model')
    expect(registrations).toHaveLength(1)

    setProviders([])
    emit('llm/adapters-updated')
    await flush()
    expect(registrations).toHaveLength(2)
  })

  it('远端面缺失时按已配置处理，不动正常席位', async () => {
    const { registrations } = harness(null)
    await flush()
    expect(registrations).toHaveLength(0)
  })
})
