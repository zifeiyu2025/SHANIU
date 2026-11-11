/**
 * DSH 权限挡位 ⇄ 宿主 `(sandbox.mode, approval.mode)` 的**唯一**映射（2026-11-11）。
 *
 * 宿主的真实安全面是两维。此前本文件只映射沙箱维、审批维另走 `set_approval_mode`，
 * 结果是「选了自主档、沙箱还是只读」——两维分头写，迟早裂成用户表达不了的中间态。
 * 现在每一档都是 `(sandbox, approval)` 的**一组绑定**：设置页、`/permission <preset>`、
 * 会话档案、resume 全部只认 id，两维由这张表一起装载（与 `permission/preset.rs` 同表）。
 *
 * 三档逐字对齐 DSH（`read-only` / `workspace-write` / `danger-full-access`）；
 * `workspace-write-auto` 是本仓库扩展（区内写入不问、越界由沙箱拒），内部就是普通一档，
 * **界面不出现「扩展」字样**。不在表里的组合（`on_failure`、`full_access`、任何混搭）
 * 派生为 {@link CUSTOM_PERMISSION_PRESET}：如实显示「自定义」，**绝不回落成具体档**——
 * 把 `full_access` 显示成 `danger-full-access` 会让用户以为自己在 `never` 档。
 */

/**
 * 「本会话的档位读取失败」的机器值。它不是挡位：目录里没有这一项。
 * 存在的唯一理由是让投影如实表达 unknown，而不是回落成某个具体档——回落会让
 * 「读不到」看起来像「只读」，用户按错误的前提点确认。
 *
 * 展示层（`ui-permission-presets` 的 `displayPermissionPreset`）负责把它翻成文案；
 * 那里有同名常量，值必须一致（`src/transport/permission-presets.spec.ts` 锁住）。
 */
export const PERMISSION_UNAVAILABLE = 'unavailable'

/**
 * 派生档：当前 `(sandbox, approval)` 组合不在预设表里。不是可选项、不进目录，
 * 也不可被写面接受；只用于如实显示「自定义」。
 */
export const CUSTOM_PERMISSION_PRESET = 'custom'

/** 一档预设绑定的两维。 */
export interface PermissionPresetBundle {
  readonly sandbox: string
  /** 宿主 `[approval] mode` 的落盘值：`never` / `on_failure` / `on_request` / `full_access`。 */
  readonly approval: string
}

/**
 * 挡位 → 两维绑定。顺序即界面与 schema 的呈现顺序（权限递增）。
 * 加一档只改这张表，目录/反查/写面全部由它派生。
 */
export const PRESET_BUNDLES: Readonly<Record<string, PermissionPresetBundle>> = {
  'read-only': { sandbox: 'read_only', approval: 'on_request' },
  'workspace-write': { sandbox: 'workspace_write', approval: 'on_request' },
  'danger-full-access': { sandbox: 'danger_full_access', approval: 'never' },
  'workspace-write-auto': { sandbox: 'workspace_write', approval: 'never' },
}

/** DSH 挡位键 → 宿主 `[sandbox] mode` 落盘值（04 §4；由 {@link PRESET_BUNDLES} 派生）。 */
export const PRESET_TO_SANDBOX: Record<string, string> = Object.fromEntries(
  Object.entries(PRESET_BUNDLES).map(([preset, bundle]) => [preset, bundle.sandbox]),
)

/**
 * 挡位全集（由绑定表派生，不另立清单）。
 * **不含** `custom` / `unavailable`：它们不是可选项。
 */
export const PERMISSION_PRESETS: readonly string[] = Object.keys(PRESET_BUNDLES)

/**
 * `permissionPreset` 缺失/非法时的回落档。**不是产品默认档**：产品默认是
 * `config.rs` 里 `SandboxMode::default()` 的 `read_only`。这里只在读不到时兜底，
 * 取权限最小的一档——宁可显示"只读"，也不显示用户从没开过的宽松档。
 */
export const DEFAULT_PERMISSION_PRESET: string = PERMISSION_PRESETS[0] ?? 'read-only'

/**
 * 挡位选项的静态目录（形状照上游 PermissionCatalog.options；name 给机器值走界面词典）。
 * **不带 `description`**：说明文案是界面文案，只能来自界面词典
 * （`ui-permission-presets` 的 `preset.*.description`）。
 */
export const PERMISSION_PRESET_OPTIONS = PERMISSION_PRESETS.map(value => ({ value, name: value }))

/**
 * DSH 挡位键 → 宿主沙箱档。
 * @returns 宿主落盘值；未知挡位返回 undefined，由调用方如实拒绝而不是猜一档。
 */
export const sandboxOfPreset = (preset: string): string | undefined => PRESET_BUNDLES[preset]?.sandbox

/** DSH 挡位键 → 宿主审批档（与沙箱档同表派生，永远成对出现）。 */
export const approvalOfPreset = (preset: string): string | undefined => PRESET_BUNDLES[preset]?.approval

/**
 * 两维 → 挡位键：**精确匹配**，不匹配返回 {@link CUSTOM_PERMISSION_PRESET}。
 *
 * 不匹配时不能挑一个"最像的"：`(workspace_write, never)` 确实等于扩展档，
 * 但 `(workspace_write, full_access)` 与它只差审批维，回落过去就等于告诉用户
 * "你在 `never` 档"——而实际是 `full_access`。
 */
export const presetOfBundle = (sandbox: string, approval: string): string => {
  for (const [preset, bundle] of Object.entries(PRESET_BUNDLES)) {
    if (bundle.sandbox === sandbox && bundle.approval === approval) return preset
  }
  return CUSTOM_PERMISSION_PRESET
}

/**
 * 设置视图 → 当前挡位。优先用宿主算好的 `permissionPreset`（它按同一张表精确匹配），
 * 旧宿主没有该字段时用两维反推——反推不命中同样如实给 `custom`。
 * @param view - `get_settings` 快照的三个安全字段（后者在前者缺失时兜底）。
 */
export const presetOfSettings = (view: {
  readonly permissionPreset?: string
  readonly sandboxMode: string
  readonly approvalMode: string
}): string => {
  const declared = view.permissionPreset
  if (declared === CUSTOM_PERMISSION_PRESET) return CUSTOM_PERMISSION_PRESET
  if (declared !== undefined && PRESET_BUNDLES[declared] !== undefined) return declared
  return presetOfBundle(view.sandboxMode, view.approvalMode)
}
