/**
 * 沙箱状态条的数据面：宿主 `sandbox_status` 命令的前端镜像。
 *
 * 字段名跟 Rust serde 的 snake_case 走（与 `transport/mcp-wire.ts` 同一约定）。
 * 这些命令是只读查询，不碰审批面；失败一律如实抛出，
 * 由界面转成可见文字——「用户看得见」是 AGENTS §11.3 的前提，不能静默吞掉。
 *
 * **没有网络面**（2026-11-10 定，对齐 DSH）：网络不是策略维度，容器一律可出网，
 * 所以没有 grants/grant/revoke 三个命令，也没有会话级网络开关。
 */
import { tauriBridge, type HostBridge } from '../transport/tauri'

/** 沙箱策略（Rust `core::config::SandboxMode` 的字符串形）。 */
export type SandboxModeWire = 'read_only' | 'workspace_write' | 'danger_full_access'

/** OS 级机制（Rust `core::config::OsSandbox` 的字符串形）。 */
export type OsSandboxWire = 'restricted_token' | 'app_container' | 'logical'

/** 文件权限后台准备的进度（Rust `sandbox::AncestorState` 的字符串形）。 */
export type AncestorStateWire = 'idle' | 'preparing' | 'ready' | 'needs_admin'

/** `sandbox_status` 的返回值（Rust `sandbox::SandboxStatus`）。 */
export interface SandboxStatusWire {
  readonly mode: SandboxModeWire
  readonly os: OsSandboxWire
  /** false = 没有 OS 级隔离，界面必须常驻标注。 */
  readonly os_enabled: boolean
  /** 强制完整度自报（Rust `sandbox::SandboxEnforcement`），与工具结果里同一判定。 */
  readonly enforcement: 'full' | 'partial'
  /** 容器 profile 名；logical 模式为空串。 */
  readonly profile: string
  /**
   * 文件权限后台准备进度。界面只在 `preparing` / `needs_admin` 时说一句话，
   * `idle` / `ready` 不占位——常态提示只会变成噪声。
   */
  readonly ancestor: AncestorStateWire
  readonly note: string
}

/** 状态条用到的全部宿主调用；测试注入脚本化实现，运行时走 `hostSandboxWire()`。 */
export interface SandboxWire {
  status(): Promise<SandboxStatusWire>
}

/** 桌面运行时的实现（命令名与 `lib.rs` 的 `generate_handler!` 逐字对应）。 */
export function hostSandboxWire(bridge: HostBridge = tauriBridge()): SandboxWire {
  return {
    status: () => bridge.invoke<SandboxStatusWire>('sandbox_status'),
  }
}
