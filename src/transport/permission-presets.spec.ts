import { describe, expect, it } from 'vitest'
import { accessEn, accessZh } from '../dsh/packages/client/ui-permission-presets/src/client/locales.ts'
import {
  displayPermissionPreset,
  displayPermissionPresetDescription,
  PERMISSION_UNAVAILABLE as PERMISSION_UNAVAILABLE_UI,
} from '../dsh/packages/client/ui-permission-presets/src/client/presentation.ts'
import { PERMISSION_PRESET_OPTIONS, PERMISSION_UNAVAILABLE, PRESET_TO_SANDBOX, PRESET_BUNDLES, CUSTOM_PERMISSION_PRESET, presetOfBundle, presetOfSettings } from './permission-presets'
import type { SandboxModeWire } from '../sandbox-status/wire'

/**
 * 宿主 `SandboxModeWire` 的穷尽表。
 *
 * 类型是 `Record<SandboxModeWire, string>`：wire 新增一档时这里缺键 → `tsc` 直接失败，
 * 逼人回来对齐 `PRESET_TO_SANDBOX`。**不用生产依赖锁**（那会把纯 wire 模块
 * `sandbox-status` 变成依赖中心），只让两侧的用例互相盯着。
 */
const WIRE_MODES: Record<SandboxModeWire, string> = {
  read_only: 'read-only',
  workspace_write: 'workspace-write',
  danger_full_access: 'danger-full-access',
}

describe('权限挡位说明行（P3-16）', () => {
  it('宿主目录不下发说明：说明是界面文案，只能来自界面词典', () => {
    // 带 description 会被 `/permission` 下拉的 detail 与芯片 tooltip 原样渲染，
    // 等于把中文硬编码进 en 界面。
    expect(PERMISSION_PRESET_OPTIONS.every(option => !('description' in option))).toBe(true)
  })

  it('按界面语言取词典：zh 保持原文，en 不再串中文', () => {
    expect(displayPermissionPresetDescription('read-only', key => accessZh[key])).toBe('只读查看，不产生任何写入')
    expect(displayPermissionPresetDescription('read-only', key => accessEn[key])).toBe('Read-only access, no writes at all')
    expect(displayPermissionPresetDescription('danger-full-access', key => accessEn[key]))
      .toBe('Not limited by the workspace boundary')
  })

  it('词典没有的挡位返回 undefined，调用方沿用宿主描述', () => {
    expect(displayPermissionPresetDescription('plain', key => accessEn[key])).toBeUndefined()
  })

  it('读取失败不显示任何具体档：哨兵值两处一致，且被翻译成「读取失败」', () => {
    // 产生方（adapter → permission-presets）与展示方（vendored presentation）各有一份
    // 常量；漂移会让芯片把 `unavailable` 当成未知挡位原名显示成 "Unavailable"（中文界面串英文）。
    expect(PERMISSION_UNAVAILABLE).toBe(PERMISSION_UNAVAILABLE_UI)
    expect(displayPermissionPreset(PERMISSION_UNAVAILABLE, PERMISSION_UNAVAILABLE, key => accessZh[key]))
      .toBe('读取失败')
    expect(displayPermissionPreset(PERMISSION_UNAVAILABLE, PERMISSION_UNAVAILABLE, key => accessEn[key]))
      .toBe('Unreadable')
    // 它不是挡位：目录里没有它，也不能被当成某一档
    expect(PERMISSION_PRESET_OPTIONS.map(option => option.value)).not.toContain(PERMISSION_UNAVAILABLE)
  })

  it('每档绑定 (sandbox, approval) 两维，且只有对齐 DSH 的三档', () => {
    // 两维必须成对：只映射沙箱维是自主档失效的成因（选了 never、沙箱还是只读）。
    expect(PRESET_BUNDLES['read-only']).toEqual({ sandbox: 'read_only', approval: 'on_request' })
    expect(PRESET_BUNDLES['workspace-write']).toEqual({ sandbox: 'workspace_write', approval: 'on_request' })
    expect(PRESET_BUNDLES['danger-full-access']).toEqual({ sandbox: 'danger_full_access', approval: 'never' })
    // 2026-11-12 用户裁定移除自造扩展档：任何把它加回拍平表的改动都必须在这里红
    expect(Object.keys(PRESET_BUNDLES)).toEqual(['read-only', 'workspace-write', 'danger-full-access'])
    expect(PRESET_BUNDLES['workspace-write-auto']).toBeUndefined()
    expect(PRESET_BUNDLES['custom']).toBeUndefined()
    // 宿主里没有 full_access 这一维值：它是 config-only，不进预设
    expect(Object.values(PRESET_BUNDLES).map(bundle => bundle.approval)).not.toContain('full_access')
  })

  it('非标准组合一律派生 custom，绝不回落成具体档', () => {
    for (const [sandbox, approval] of [
      ['workspace_write', 'never'],
      ['workspace_write', 'on_failure'],
      ['workspace_write', 'full_access'],
      ['read_only', 'never'],
      ['danger_full_access', 'on_request'],
      ['danger_full_access', 'full_access'],
    ] as const) {
      expect(presetOfBundle(sandbox, approval)).toBe(CUSTOM_PERMISSION_PRESET)
    }
    // 宿主显式给 custom 时原样保留；旧宿主没这个字段时按两维反推
    expect(presetOfSettings({ permissionPreset: CUSTOM_PERMISSION_PRESET, sandboxMode: 'workspace_write', approvalMode: 'never' }))
      .toBe(CUSTOM_PERMISSION_PRESET)
    // 扩展档删掉后，`workspace_write + never` 只能如实显示「自定义」
    expect(presetOfSettings({ sandboxMode: 'workspace_write', approvalMode: 'never' }))
      .toBe(CUSTOM_PERMISSION_PRESET)
    // custom 不可选：目录里没有它
    expect(PERMISSION_PRESET_OPTIONS.map(option => option.value)).not.toContain(CUSTOM_PERMISSION_PRESET)
  })

  it('派生档有词典文案；已移除的扩展档不再命中专门文案', () => {
    expect(displayPermissionPreset('custom', 'custom', key => accessZh[key])).toBe('自定义')
    // 扩展档的词典键已随档位一起删除：命中不了就是「没有这一档」，不是漏翻
    expect(displayPermissionPresetDescription('workspace-write-auto', key => accessZh[key])).toBeUndefined()
    expect(displayPermissionPresetDescription('workspace-write-auto', key => accessEn[key])).toBeUndefined()
  })

  it('PRESET_TO_SANDBOX 与 SandboxModeWire 的值集合必须相等', () => {
    // 正向：映射产出的每个宿主档都必须是合法 wire 值（多一档 = 写进去的档宿主不认）
    for (const mode of Object.values(PRESET_TO_SANDBOX)) {
      expect(Object.keys(WIRE_MODES)).toContain(mode)
    }
    // 反向：每个 wire 值都要能在映射里找到挡位（少一档 = 界面上少一个可选项却没报错）
    for (const [mode, preset] of Object.entries(WIRE_MODES)) {
      expect(PRESET_TO_SANDBOX[preset]).toBe(mode)
    }
  })
})
