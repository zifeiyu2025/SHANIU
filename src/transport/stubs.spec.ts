import { describe, expect, it, vi } from 'vitest'
import { StubRemoteAccess } from './stubs'
import { remote } from './shims/remote-module'

it('未接线的 remote 描述符读取时显式失败，不伪造空贡献', () => {
  expect(() => remote.package).toThrow('未接线')
  expect(() => remote.descriptors).toThrow('未接线')
})

/** 最小 ctx：Service 构造只要求 reflect.provide，StubRemoteAccess 另写 slots.provideRoot。 */
function harness(allow: boolean): StubRemoteAccess {
  const ctx = {
    reflect: { provide: () => () => {} },
    slots: { provideRoot: () => () => {} },
  }
  return new StubRemoteAccess(ctx as never, allow)
}

describe('StubRemoteAccess.whenAccess', () => {
  it('授权时保留回调 yield 出的注册，直到卸载才逆序释放', () => {
    const calls: string[] = []
    const dispose = harness(true).whenAccess('schedule/catalog', function* () {
      calls.push('register-a')
      yield () => { calls.push('dispose-a') }
      calls.push('register-b')
      yield () => { calls.push('dispose-b') }
    } as never)

    // 注册完必须仍然生效——曾经在这里逐个立即调用，等于「挂上就拆」
    expect(calls).toEqual(['register-a', 'register-b'])
    dispose()
    expect(calls).toEqual(['register-a', 'register-b', 'dispose-b', 'dispose-a'])
  })

  it('未授权时不执行回调', () => {
    const callback = vi.fn()
    const dispose = harness(false).whenAccess('schedule/catalog', callback as never)
    expect(callback).not.toHaveBeenCalled()
    dispose()
    expect(callback).not.toHaveBeenCalled()
  })

  it('普通函数形式的单个 disposer 在卸载时执行且只执行一次（幂等）', () => {
    const disposeEffect = vi.fn()
    const dispose = harness(true).whenAccess('schedule/catalog', () => disposeEffect)
    expect(disposeEffect).not.toHaveBeenCalled()
    dispose()
    dispose()
    expect(disposeEffect).toHaveBeenCalledTimes(1)
  })
})
