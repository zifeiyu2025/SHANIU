/**
 * 沙箱状态条的数据面：3 组宿主命令的前端镜像。
 *
 * 字段名跟 Rust serde 的 snake_case 走（与 `transport/mcp-wire.ts` 同一约定）。
 * 这些命令是只读查询 + 会话级授权，不碰审批面；失败一律如实抛出，
 * 由界面转成可见文字——「用户看得见」是 AGENTS §11.3 的前提，不能静默吞掉。
 */
import { tauriBridge, type HostBridge } from '../transport/tauri'

/** 沙箱策略（Rust `core::config::SandboxMode` 的字符串形）。 */
export type SandboxModeWire = 'read_only' | 'workspace_write' | 'danger_full_access'

/** OS 级机制（Rust `core::config::OsSandbox` 的字符串形）。 */
export type OsSandboxWire = 'app_container' | 'logical'

/** `sandbox_status` 的返回值（Rust `sandbox::SandboxStatus`）。 */
export interface SandboxStatusWire {
  readonly mode: SandboxModeWire
  readonly os: OsSandboxWire
  /** false = 没有 OS 级隔离，界面必须常驻标注。 */
  readonly os_enabled: boolean
  /** 容器 profile 名；logical 模式为空串。 */
  readonly profile: string
  readonly note: string
}

/** 一条会话级网络授权（Rust `sandbox::network::NetworkGrant`）。 */
export interface NetworkGrantWire {
  readonly session_id: string
  readonly granted_by: string
  readonly granted_at_ms: number
  readonly scope: string
}

/** 状态条用到的全部宿主调用；测试注入脚本化实现，运行时走 `hostSandboxWire()`。 */
export interface SandboxWire {
  status(): Promise<SandboxStatusWire>
  grants(): Promise<readonly NetworkGrantWire[]>
  grant(sessionId: string): Promise<NetworkGrantWire>
  revoke(sessionId: string): Promise<boolean>
}

/** 桌面运行时的实现（命令名与 `lib.rs` 的 `generate_handler!` 逐字对应）。 */
export function hostSandboxWire(bridge: HostBridge = tauriBridge()): SandboxWire {
  return {
    status: () => bridge.invoke<SandboxStatusWire>('sandbox_status'),
    grants: () => bridge.invoke<NetworkGrantWire[]>('sandbox_network_grants'),
    grant: (sessionId) =>
      bridge.invoke<NetworkGrantWire>('sandbox_network_grant', { sessionId }),
    revoke: (sessionId) => bridge.invoke<boolean>('sandbox_network_revoke', { sessionId }),
  }
}
