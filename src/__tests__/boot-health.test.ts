/**
 * boot 级装载健康：真实 features + 真实 ShortcutsService 起一遍壳，
 * 钉住两件曾经静默失效的事实：
 * 1. 没有任何插件 fiber 停在 FAILED（cordis 只标记、不报错）；
 * 2. 会话头 utilities 槽位里有 SHANIU 的资源管理器入口。
 *
 * 反向对照：`header-actions` 若再注册已被 `ui-open-in-app` 占用的
 * `workspace.openLocal`，ShortcutsRegistry 会抛重复错误，此用例两处断言同时红。
 */
import type { Fiber, FiberState } from '@deepseek-ai/cordis'
import { describe, expect, it } from 'vitest'

import { bootShaniu } from '../dsh-boot'

/** cordis `FiberState.FAILED`（const enum 在 isolatedModules 下不能按值导入）。 */
const FIBER_STATE_FAILED: FiberState = 3

/** cordis 未暴露 error getter；FAILED fiber 的原因只在私有字段里（fiber.ts）。 */
function failureReason(fiber: Fiber): string {
  const error = (fiber as unknown as { _error?: unknown })._error
  return error instanceof Error ? error.message : String(error)
}

describe('boot 装载健康', () => {
  it('没有 FAILED fiber，且会话头资源管理器入口已注册', async () => {
    const container = document.createElement('div')
    document.body.append(container)
    const handle = await bootShaniu(container, { mount: false })
    try {
      const failed = [...handle.ctx.registry.values()]
        .flatMap(runtime => [...runtime.fibers])
        .filter(fiber => fiber.state === FIBER_STATE_FAILED)
      expect(failed.map(failureReason)).toEqual([])
      const ids = handle.ctx.slots
        .entries('conversation.session.header.utilities')
        .map(entry => entry.options.id)
      expect(ids).toContain('shaniu-explorer')
    } finally {
      // 只测装配结果，不等拆卸：mock 传输的流不会自然结束，dispose 在这里会一直等下去。
      void handle.dispose()
      container.remove()
    }
  }, 60_000)
})
