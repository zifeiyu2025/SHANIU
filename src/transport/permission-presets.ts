/**
 * DSH 一维权限挡位 ⇄ 宿主 `sandbox.mode` 的**唯一**映射（产品拍板，2026-11-07）。
 *
 * 宿主的安全面是 `sandbox.mode × approval.mode` 两维，但裁决矩阵（permission/policy.rs
 * 的 decide）当前只消费沙箱维，所以挡位只映射沙箱档、approval_mode 不动。目录不含
 * 上游实验档 `auto`：它是 danger-full-access × ask 的组合，在"裁决不看 approval"的
 * 宿主里与完全权限行为相同，挂出来是一枚假挡位。挡位语义是**全局**的（写 config.toml，
 * 不随会话走）：agent_loop 在任务启动时固化 SessionPolicy，进行中回合不受切换影响。
 *
 * 为什么单独一个模块：这张表有三个消费面——`remote.permissionPresets` 的目录、
 * `/permission <preset>` 命令的执行、设置页 `permission` 域的读视图与写面。三面各写
 * 一份映射的结果是"芯片显示的档"和"写进 config.toml 的档"迟早不是同一档，
 * 所以全部从这里派生（`presetOfSandbox` / `sandboxOfPreset` 是唯一的双向翻译）。
 */

/** DSH 挡位键 → 宿主 `[sandbox] mode` 落盘值（04 §4）。 */
export const PRESET_TO_SANDBOX: Record<string, string> = {
  'read-only': 'read_only',
  'workspace-write': 'workspace_write',
  'danger-full-access': 'danger_full_access',
}

/** 宿主 `[sandbox] mode` → DSH 挡位键（反查表，与 {@link PRESET_TO_SANDBOX} 同源生成）。 */
export const SANDBOX_TO_PRESET: Record<string, string> = Object.fromEntries(
  Object.entries(PRESET_TO_SANDBOX).map(([preset, mode]) => [mode, preset]),
)

/**
 * 挡位全集，顺序即界面与 schema 的呈现顺序（权限递增：只读 → 工作区内 → 完全）。
 * 由映射表派生，不另立清单——顺序即表顺序，加一档只改上面那张表。
 */
export const PERMISSION_PRESETS: readonly string[] = Object.keys(PRESET_TO_SANDBOX)

/**
 * `presetOfSandbox` 认不出沙箱档时的回落档。`config.rs` 里 `SandboxMode` 的
 * `#[default]` 就是 ReadOnly，表首项与之同义；回落取权限最小的一档——宁可显示
 * "只读"，也不显示一个用户从没开过的宽松档。
 */
export const DEFAULT_PERMISSION_PRESET: string = PERMISSION_PRESETS[0] ?? 'read-only'

/**
 * 挡位选项的静态目录（形状照上游 PermissionCatalog.options；name 给机器值走界面词典）。
 * 只服务于 `remote.permissionPresets` 的 catalog 应答。
 */
export const PERMISSION_PRESET_OPTIONS = PERMISSION_PRESETS.map(value => ({
  value,
  name: value,
  description: value === 'read-only'
    ? '只读查看，不产生任何写入'
    : value === 'workspace-write'
      ? '仅在工作区内读写'
      : '不受工作区边界限制',
}))

/**
 * 宿主沙箱档 → DSH 挡位键。读不到/认不出的档回落 {@link DEFAULT_PERMISSION_PRESET}：
 * UI 的芯片必须有一个确定态，但绝不猜成比实际更宽松的一档。
 * @param mode - `get_settings` 回显的 `sandboxMode`（read_only / workspace_write / danger_full_access）。
 * @returns 三档之一，保证落在 {@link PERMISSION_PRESETS} 里。
 */
export const presetOfSandbox = (mode: string): string => SANDBOX_TO_PRESET[mode] ?? DEFAULT_PERMISSION_PRESET

/**
 * DSH 挡位键 → 宿主沙箱档。
 * @param preset - 界面给来的挡位键（可能来自旧客户端或手工输入）。
 * @returns 宿主落盘值；未知挡位返回 undefined，由调用方如实拒绝而不是猜一档。
 */
export const sandboxOfPreset = (preset: string): string | undefined => PRESET_TO_SANDBOX[preset]
