// @vitest-environment jsdom
/**
 * 模型席位契约测试。
 *
 * 两件事：
 *  1. 空态/加载态的 DOM 与文案对齐 DSH `ModelSelect`（不是自造的 Button）；
 *  2. **规格一致**：本组件 CSS 的几何必须与同槽位原件（ModelSelect）和同排
 *     访问模式 chip（PermissionSelect）逐项相等——这是「不是各写各的」的机检。
 *     上游文件就用仓库里的 vendored 副本，不依赖外部路径。
 */
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { describe, expect, it, vi } from 'vitest'
import type { ReactNode } from 'react'
import { render, screen } from '@testing-library/react'
import { ModelConfigSeat } from './ModelConfigSeat'
import { zh } from './locales'

const VENDOR = join(import.meta.dirname, '..', 'dsh', 'packages', 'client')
const MODEL_CSS = join(VENDOR, 'ui-model-selection', 'src', 'client', 'ModelSelect.module.css')
const PERMISSION_CSS = join(VENDOR, 'ui-permission-presets', 'src', 'client', 'PermissionSelect.module.css')
const SEAT_CSS = join(import.meta.dirname, 'model-seat.css')

/** 直译词典（只关心键是否被用到）。 */
function translate(key: string): string {
  return (zh as Record<string, string>)[key] ?? key
}

/** 取出某个选择器的声明块，返回「属性 → 值」（先剥注释，避免注释里的冒号污染键名）。 */
function declarations(raw: string, selector: string): Map<string, string> {
  const css = raw.replace(/\/\*[\s\S]*?\*\//g, '')
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const match = css.match(new RegExp(`(?:^|\\n)\\s*${escaped}\\s*\\{([^}]*)\\}`))
  if (match === null) throw new Error(`selector not found: ${selector}`)
  const map = new Map<string, string>()
  for (const chunk of (match[1] ?? '').split(';')) {
    const at = chunk.indexOf(':')
    if (at < 0) continue
    map.set(chunk.slice(0, at).trim(), chunk.slice(at + 1).trim())
  }
  return map
}

function seatElement(inject: { open: () => void, loading: boolean }): ReactNode {
  const Seat = ModelConfigSeat as unknown as (props: Record<string, unknown>) => ReactNode
  return <Seat t={translate} open={inject.open} loading={inject.loading} />
}

describe('src/model-config：席位形态', () => {
  it('空态：文案「请选择模型」，有模型图标与 chevron，点击打开设置', () => {
    const open = vi.fn()
    render(seatElement({ open, loading: false }))

    const seat = screen.getByRole('button', { name: zh['seat.aria'] })
    expect(seat.textContent).toContain('请选择模型')
    expect(seat.getAttribute('aria-busy')).toBeNull()
    expect(seat.querySelector('svg[data-icon*="data-outline"]')).toBeTruthy()
    expect(seat.querySelector('svg[data-icon*="chevron-down"]')).toBeTruthy()

    seat.click()
    expect(open).toHaveBeenCalledTimes(1)
  })

  it('加载态：文案「正在加载模型…」，aria-busy，且用状态点替代 chevron', () => {
    render(seatElement({ open: () => {}, loading: true }))

    const seat = screen.getByRole('button', { name: zh['seat.aria'] })
    expect(seat.textContent).toContain('正在加载模型…')
    expect(seat.getAttribute('aria-busy')).toBe('true')
    expect(seat.querySelector('svg[data-icon*="chevron-down"]')).toBeNull()
  })

  it('几何与同槽位 DSH 原件、同排访问模式 chip 逐项一致', () => {
    const seat = declarations(readFileSync(SEAT_CSS, 'utf8'), '.shaniu-model-seat')
    const model = declarations(readFileSync(MODEL_CSS, 'utf8'), '.trigger')
    const permission = declarations(readFileSync(PERMISSION_CSS, 'utf8'), '.trigger')

    // 与模型 chip：除名字外应完全同规格（含字重 400）。
    for (const key of [
      'display', 'align-items', 'gap', 'height', 'padding', 'border', 'border-radius',
      'background', 'color', 'font-size', 'line-height', 'font-weight',
    ]) {
      expect(seat.get(key), `declaration ${key}`).toBe(model.get(key))
    }

    // 与访问模式 chip：几何必须对得上（字重上游两者本就不同，不在此列）。
    for (const key of ['gap', 'height', 'padding', 'border-radius', 'font-size', 'line-height']) {
      expect(seat.get(key), `geometry ${key}`).toBe(permission.get(key))
    }
  })
})
