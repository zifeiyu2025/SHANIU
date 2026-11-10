import { describe, expect, it } from 'vitest'
import { accessEn, accessZh } from '../dsh/packages/client/ui-permission-presets/src/client/locales.ts'
import { displayPermissionPresetDescription } from '../dsh/packages/client/ui-permission-presets/src/client/presentation.ts'
import { PERMISSION_PRESET_OPTIONS } from './permission-presets'

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
})
