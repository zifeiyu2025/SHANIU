import { describe, expect, it } from 'vitest'
import { accessEn, accessZh } from '../dsh/packages/client/ui-permission-presets/src/client/locales.ts'
import {
  displayPermissionPreset,
  displayPermissionPresetDescription,
  PERMISSION_UNAVAILABLE as PERMISSION_UNAVAILABLE_UI,
} from '../dsh/packages/client/ui-permission-presets/src/client/presentation.ts'
import { PERMISSION_PRESET_OPTIONS, PERMISSION_UNAVAILABLE } from './permission-presets'

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
})
