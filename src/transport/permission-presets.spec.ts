import { describe, expect, it } from 'vitest'
import { accessEn, accessZh } from '../dsh/packages/client/ui-permission-presets/src/client/locales.ts'
import {
  displayPermissionPreset,
  displayPermissionPresetDescription,
  PERMISSION_UNAVAILABLE as PERMISSION_UNAVAILABLE_UI,
} from '../dsh/packages/client/ui-permission-presets/src/client/presentation.ts'
import { PERMISSION_PRESET_OPTIONS, PERMISSION_UNAVAILABLE, PRESET_TO_SANDBOX } from './permission-presets'
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
