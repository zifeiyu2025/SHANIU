/**
 * DSH 一维权限挡位 ⇄ 宿主 `sandbox.mode` 的**唯一**映射（产品拍板，2026-11-07）。
 *
 * 宿主的安全面是 `sandbox.mode × approval.mode` 两维。SH-120 起裁决矩阵（permission/policy.rs
 * 的 decide）两维都消费，但本挡位只映射沙箱档：审批档由 `set_approval_mode` 在会话内切换
 * （下一 turn 生效），与"全局默认写 config.toml"是两条路。目录不含上游实验档 `auto`
 * （danger-full-access × ask）：在宿主里它等于"无沙箱但执行仍逐次问"，与完全权限不等价，
 * 挂出来会让用户以为拿到了免确认。挡位语义是**全局默认**的（写 config.toml）；单个会话的
 * 审批档可在运行中切换，任务启动时从 config 重置，不跨任务泄漏。
 *
 * 为什么单独一个模块：这张表有三个消费面——`remote.permissionPresets` 的目录、
 * `/permission <preset>` 命令的执行、设置页 `permission` 域的读视图与写面。三面各写
 * 一份映射的结果是"芯片显示的档"和"写进 config.toml 的档"迟早不是同一档，
 * 所以全部从这里派生（`presetOfSandbox` / `sandboxOfPreset` 是唯一的双向翻译）。
 */

/**
 * 「本会话的档位读取失败」的机器值。它不是挡位：`presetOfSandbox` 永远不会产出它，
 * 目录里也没有这一项。存在的唯一理由是让投影如实表达 unknown，而不是回落成某个
 * 具体档——回落会让「读不到」看起来像「只读」，用户按错误的前提点确认。
 *
 * 展示层（`ui-permission-presets` 的 `displayPermissionPreset`）负责把它翻成文案；
 * 那里有同名常量，值必须一致（`src/transport/permission-presets.spec.ts` 锁住）。
 */
export const PERMISSION_UNAVAILABLE = 'unavailable'

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
 * `presetOfSandbox` 认不出沙箱档时的回落档。**不是产品默认档**：产品默认是
 * `config.rs` 里 `SandboxMode::default()` 的 `read_only`（2026-11-10 起，与 DSH
 * 一致；此前曾短暂默认 `workspace_write`）。这里只在配置读不到/值非法时兜底，
 * 取权限最小的一档——宁可显示"只读"，也不显示一个用户从没开过的宽松档。
 */
export const DEFAULT_PERMISSION_PRESET: string = PERMISSION_PRESETS[0] ?? 'read-only'

/**
 * 挡位选项的静态目录（形状照上游 PermissionCatalog.options；name 给机器值走界面词典）。
 * 只服务于 `remote.permissionPresets` 的 catalog 应答。
 *
 * **不带 `description`**：说明文案是界面文案，只能来自界面词典
 * （`ui-permission-presets` 的 `preset.*.description`）。写在这里会被 `/permission`
 * 下拉的 detail 与芯片 tooltip 原样渲染，等于把中文硬编码进 en 界面。
 */
export const PERMISSION_PRESET_OPTIONS = PERMISSION_PRESETS.map(value => ({ value, name: value }))

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
