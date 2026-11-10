/**
 * 插件装载失败上报：事件驱动（`internal/status`）替代 boot 收尾快照。
 *
 * 快照的盲区：`apply` 里 `await` 之后才抛的异常，落成 FAILED 的时机晚于「两个宏任务 +
 * 一个微任务」；boot 完成之后才失败的也永远扫不到。这里用「快照时点上报数仍为 0、
 * 事件随后补上」来固定这个先后关系，并用一次正常 boot → dispose 证明卸载期的
 * disposer 抛错不会被记成装载失败。
 */
import { Context } from '@deepseek-ai/cordis'
import { afterEach, describe, expect, it, vi } from 'vitest'

import { bootShaniu, dumpPendingPlugins, watchPluginFailures } from '../dsh-boot'

/** 上报过的「插件装载失败」条目（格式串、标签、原因）。 */
function loadFailures(error: { mock: { calls: unknown[][] } }): unknown[][] {
  return error.mock.calls.filter(call => String(call[0]).includes('插件装载失败'))
}

afterEach(() => {
  vi.restoreAllMocks()
})

describe('插件装载失败：事件驱动上报', () => {
  it('apply 里 await 之后才抛的失败也会上报（boot 收尾快照会漏）', async () => {
    const ctx = new Context()
    const labels = new Map<number, string>()
    const error = vi.spyOn(ctx.logger, 'error').mockImplementation(() => {})
    watchPluginFailures(ctx, labels, () => false)

    const fiber = ctx.plugin({
      inject: [],
      async apply() {
        // 比 boot 收尾的两个 setTimeout(0) 更晚：快照那一刻它还没抛。
        await new Promise<void>(resolve => { setTimeout(resolve, 30) })
        throw new Error('晚到的装载失败')
      },
    })
    if (fiber.uid !== null) labels.set(fiber.uid, 'features[99] late-throw')

    // boot 收尾的等价时点：两个宏任务 + 一个微任务（旧实现在这里扫一次就结束）。
    await new Promise<void>(resolve => { setTimeout(resolve, 0) })
    await new Promise<void>(resolve => { setTimeout(resolve, 0) })
    await Promise.resolve()
    expect(loadFailures(error)).toHaveLength(0) // 旧实现到此为止 = 永久漏报

    await expect(fiber.await()).rejects.toThrow('晚到的装载失败')
    await Promise.resolve()
    const reported = loadFailures(error)
    expect(reported).toHaveLength(1)
    expect(String(reported[0]?.[1])).toContain('features[99] late-throw')
    expect(String(reported[0]?.[2])).toContain('晚到的装载失败')
    await ctx.fiber.dispose()
  })

  it('正常 boot → dispose 全程零条「插件装载失败」', async () => {
    const error = vi.spyOn(console, 'error').mockImplementation(() => {})
    // 走桌面真实数据源分支：mock 分支的流不会自然结束，dispose 会一直等（见 boot-health）。
    ;(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
      invoke: async (command: string) => {
        if (command === 'gateway_info') throw new Error('本测试没有 loopback gateway')
        if (command === 'get_product_dirs' || command === 'get_settings') return {}
        if (command === 'get_workspace_flags') {
          return { archivedSessionIds: [], pinnedSessionIds: [], sessionOrder: [] }
        }
        return []
      },
    }
    const originalFetch = globalThis.fetch
    const container = document.createElement('div')
    document.body.append(container)

    try {
      const handle = await bootShaniu(container, { mount: false })
      await handle.dispose()
    } finally {
      ;(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = undefined
      globalThis.fetch = originalFetch
      delete (globalThis as { dshDesktop?: unknown }).dshDesktop
      delete (globalThis as { __DSH_FILE_UPLOAD__?: unknown }).__DSH_FILE_UPLOAD__
      container.remove()
    }

    const messages = error.mock.calls.map(call => call.map(String).join(' '))
    expect(messages.filter(text => text.includes('插件装载失败'))).toHaveLength(0)
  }, 60_000)

  it('拆卸窗口内的失败不上报；同样的失败在装载期会报（标志的反向对照）', async () => {
    const lateThrow = {
      inject: [],
      apply: async () => { throw new Error('装载失败一例') },
    }

    const booting = new Context()
    const bootingLog = vi.spyOn(booting.logger, 'error').mockImplementation(() => {})
    watchPluginFailures(booting, new Map(), () => false)
    const bootingFiber = booting.plugin(lateThrow)
    await expect(bootingFiber.await()).rejects.toThrow('装载失败一例')
    expect(loadFailures(bootingLog)).toHaveLength(1)
    await booting.fiber.dispose()

    const disposing = new Context()
    const disposingLog = vi.spyOn(disposing.logger, 'error').mockImplementation(() => {})
    let inTeardown = false
    watchPluginFailures(disposing, new Map(), () => inTeardown)
    const disposingFiber = disposing.plugin(lateThrow)
    // 失败落在拆卸窗口内：不应记成「插件装载失败」。
    inTeardown = true
    await expect(disposingFiber.await()).rejects.toThrow('装载失败一例')
    expect(loadFailures(disposingLog)).toHaveLength(0)
    await disposing.fiber.dispose()
  })

  it('开发态 dump 列出非 ACTIVE 插件与缺失注入，ACTIVE 的不列', async () => {
    const info = vi.spyOn(console, 'info').mockImplementation(() => {})
    const ctx = new Context()
    const labels = new Map<number, string>()

    const active = ctx.plugin({ inject: [], apply() {} })
    await active.await()
    const pending = ctx.plugin({ inject: ['noSuchService'], apply() {} })
    if (active.uid !== null) labels.set(active.uid, 'features[1] active')
    if (pending.uid !== null) labels.set(pending.uid, 'features[404] needs-noSuchService')

    dumpPendingPlugins(ctx, labels)

    expect(info).toHaveBeenCalledTimes(1)
    const text = String(info.mock.calls[0]?.[1])
    expect(text).toContain('features[404] needs-noSuchService')
    expect(text).toContain('noSuchService')
    expect(text).not.toContain('features[1] active')
    await ctx.fiber.dispose()
  })
})
