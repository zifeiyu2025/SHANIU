/**
 * boot 的全局补丁必须能被 dispose 撤回。
 *
 * 三个 `install*`（桌面文件动作、open-in-app 路由、附件上传）都直接改 `globalThis`：
 * `globalThis.fetch` 被包裹成 routed、`dshDesktop.__SHANIU__` 被放上桌面面、
 * `__DSH_FILE_UPLOAD__` 被挂上钩子。它们不在 cordis 上下文里，`fiber.dispose()` 不管，
 * 以前的 dispose 也没收返回值——结果 `fetch` 永久是包装后的引用，同一进程重复 boot
 * 会一层套一层的包装。
 *
 * 这里断言的是**引用相同**，不是「行为一样」：行为一样可能来自包装函数透传，
 * 引用相同才能证明补丁真的被撤掉了。
 */
import { afterEach, describe, expect, it } from 'vitest'

import { bootShaniu } from '../dsh-boot'

/**
 * 桌面判定走 `__TAURI_INTERNALS__` 存在性。这里给的假宿主只求「不炸」：
 * 列表类命令回空数组、目录/设置回空对象、gateway 直接拒绝（没有 loopback 服务，
 * 让事件面走 Tauri 广播兜底）。本用例只关心全局补丁的安装与还原，不验证数据面。
 */
function installTauriStub(): void {
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
}

function clearGlobalPatches(fetchBefore: typeof fetch): void {
  delete (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__
  globalThis.fetch = fetchBefore
  delete (globalThis as { dshDesktop?: unknown }).dshDesktop
  delete (globalThis as { __DSH_FILE_UPLOAD__?: unknown }).__DSH_FILE_UPLOAD__
}

describe('boot 全局补丁的还原', () => {
  const originalFetch = globalThis.fetch

  afterEach(() => {
    // 用例失败也不把补丁漏给下一个测试文件。
    clearGlobalPatches(originalFetch)
  })

  it('boot 期间装上钩子；dispose 后 fetch 回到原引用，其余全局钩子一并撤回', async () => {
    installTauriStub()
    const container = document.createElement('div')
    document.body.append(container)

    const handle = await bootShaniu(container, { mount: false })
    try {
      // 先证明补丁确实装上了，否则后面的「引用相同」是空转。
      expect(globalThis.fetch).not.toBe(originalFetch)
      expect((globalThis as { dshDesktop?: unknown }).dshDesktop).toBeTruthy()
      expect((globalThis as { __DSH_FILE_UPLOAD__?: unknown }).__DSH_FILE_UPLOAD__).toBeTruthy()
    } finally {
      await handle.dispose()
      container.remove()
    }

    expect(globalThis.fetch).toBe(originalFetch)
    expect((globalThis as { dshDesktop?: unknown }).dshDesktop).toBeUndefined()
    expect((globalThis as { __DSH_FILE_UPLOAD__?: unknown }).__DSH_FILE_UPLOAD__).toBeUndefined()
  }, 60_000)
})
