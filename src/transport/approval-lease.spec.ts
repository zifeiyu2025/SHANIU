// @vitest-environment jsdom
/**
 * 审批租约契约测试。
 *
 * 三个失败模式各自对应一条用例：
 *  1. 心跳停了但租约还在续 → 后端以为有人看，审批永远等不到回执；
 *  2. 心跳返回 false（宿主重启/看门狗清理）却继续沉默续约 → 所有审批被拒；
 *  3. 页面卸载/刷新不退租，只靠 TTL → 刷新窗口内新页面还没 attach 就被判无人。
 */
import { afterEach, describe, expect, it, vi } from 'vitest'
import { APPROVAL_LEASE_HEARTBEAT_MS, startApprovalLease } from './approval-lease'
import type { HostBridge } from './tauri'

/** 记录命令序列的桥；`alive` 控制心跳返回值。 */
function fakeBridge(alive: () => boolean): { bridge: HostBridge; calls: string[] } {
  const calls: string[] = []
  return {
    calls,
    bridge: {
      async invoke<T>(command: string): Promise<T> {
        calls.push(command)
        return (command === 'approval_heartbeat' ? alive() : undefined) as T
      },
      listenEvents: () => () => {},
    },
  }
}

afterEach(() => {
  vi.useRealTimers()
})

describe('src/transport/approval-lease：续约', () => {
  it('启动即登记，之后按间隔心跳（默认 5s）', async () => {
    vi.useFakeTimers()
    const fake = fakeBridge(() => true)
    expect(APPROVAL_LEASE_HEARTBEAT_MS).toBe(5000)
    const lease = startApprovalLease(fake.bridge, { viewerId: 'viewer-1' })
    expect(fake.calls).toEqual(['approval_attach'])

    await vi.advanceTimersByTimeAsync(5000)
    expect(fake.calls).toEqual(['approval_attach', 'approval_heartbeat'])
    await vi.advanceTimersByTimeAsync(5000)
    expect(fake.calls).toEqual(['approval_attach', 'approval_heartbeat', 'approval_heartbeat'])
    lease.dispose()
  })

  it('心跳发现租约已不存在时立刻重新登记，而不是沉默续约', async () => {
    vi.useFakeTimers()
    let alive = true
    const fake = fakeBridge(() => alive)
    const lease = startApprovalLease(fake.bridge, { viewerId: 'viewer-1', heartbeatMs: 1000 })
    await vi.advanceTimersByTimeAsync(1000)
    expect(fake.calls).toEqual(['approval_attach', 'approval_heartbeat'])
    alive = false
    await vi.advanceTimersByTimeAsync(1000)
    // 心跳 false → 下一个动作必须是 attach（否则这条租约永远续不上）。
    expect(fake.calls).toEqual([
      'approval_attach', 'approval_heartbeat', 'approval_heartbeat', 'approval_attach',
    ])
    lease.dispose()
  })

  it('dispose 退租并停止续约；重复调用不会重复退租', async () => {
    vi.useFakeTimers()
    const fake = fakeBridge(() => true)
    const lease = startApprovalLease(fake.bridge, { viewerId: 'viewer-1', heartbeatMs: 1000 })
    lease.dispose()
    lease.dispose()
    expect(fake.calls).toEqual(['approval_attach', 'approval_detach'])
    await vi.advanceTimersByTimeAsync(5000)
    expect(fake.calls).toEqual(['approval_attach', 'approval_detach'])
  })

  it('pagehide 触发退租（刷新/关窗走这条路），WebView 断开也不抛出', async () => {
    vi.useFakeTimers()
    const calls: string[] = []
    const bridge: HostBridge = {
      invoke: async <T>(command: string): Promise<T> => {
        calls.push(command)
        if (command === 'approval_detach') throw new Error('channel closed')
        return undefined as T
      },
      listenEvents: () => () => {},
    }
    startApprovalLease(bridge, { viewerId: 'viewer-1', heartbeatMs: 1000 })
    window.dispatchEvent(new Event('pagehide'))
    expect(calls).toEqual(['approval_attach', 'approval_detach'])
    // 退租失败不能再冒出未处理的 rejection。
    await vi.advanceTimersByTimeAsync(0)
    await vi.advanceTimersByTimeAsync(5000)
    expect(calls).toEqual(['approval_attach', 'approval_detach'])
  })
})
