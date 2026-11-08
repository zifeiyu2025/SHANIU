/**
 * 沙箱可见面的共享状态（两个可见面必须一致）。
 *
 * 为什么需要它：状态条在 `shell.bottom`（根作用域，**拿不到**会话 id），而网络授权
 * 按会话生效，入口必须放在能拿到 `sessionId` 的会话头部槽位。两处看到的授权列表
 * 若是各自拉一份，撤销后就会一边变一边不变——所以授权列表与反馈放这里，两边订阅。
 *
 * 刻意不做的事：不缓存 `sandbox_status`（那份只在状态条用），不落盘，不做跨进程
 * 共享——授权真值始终在后端，这里只是它的镜像。
 */
import { useSyncExternalStore } from 'react'
import type { NetworkGrantWire, SandboxWire } from './wire'

/** 一次操作后的可见反馈。 */
export interface SandboxNotice {
  readonly tone: 'success' | 'error'
  readonly text: string
}

/** 共享快照。 */
export interface SandboxState {
  readonly grants: readonly NetworkGrantWire[]
  readonly notice: SandboxNotice | null
}

let state: SandboxState = { grants: [], notice: null }
const listeners = new Set<() => void>()

function emit(): void {
  for (const listener of listeners) listener()
}

function setState(next: SandboxState): void {
  state = next
  emit()
}

/** 订阅共享快照（`useSyncExternalStore` 的 subscribe）。 */
export function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => { listeners.delete(listener) }
}

/** 读当前快照（`useSyncExternalStore` 的 getSnapshot）。 */
export function getState(): SandboxState {
  return state
}

/** 组件侧读共享快照。 */
export function useSandboxState(): SandboxState {
  return useSyncExternalStore(subscribe, getState, getState)
}

/** 写一条可见反馈（null 清除）。 */
export function setNotice(notice: SandboxNotice | null): void {
  setState({ ...state, notice })
}

/**
 * 从后端重拉授权列表。
 *
 * 失败必须变成可见反馈而不是静默：列表过期会让用户以为"还开着"，那正是这条红线
 * 要防的情形。成功路径不产生反馈（避免每次刷新都刷一条噪音）。
 * @param wire - 宿主命令实现。
 */
export async function refreshGrants(wire: SandboxWire): Promise<void> {
  try {
    const grants = await wire.grants()
    setState({ ...state, grants })
  } catch (error: unknown) {
    setState({ grants: state.grants, notice: { tone: 'error', text: String(error) } })
  }
}
