// @vitest-environment jsdom
/**
 * 自有品牌插件的接缝契约测试。
 *
 * 这里不复建 SlotRegistry 的声明引导链（'root' 由谁声明属于 vendored 内部装配，
 * 复刻它只会让测试跟着上游私有实现走）。测的是本插件自己承诺的两件事：
 * 它等的是 `sidebar.brand.name` 这个键，以及槽位就位时填进去的组件就是 ShaniuBrandName。
 * 端到端是否真的换掉 "DSH 本地构建"，由桌面窗口截图验收（见交付说明）。
 */
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { beforeEach, describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as brandApply, inject as brandInject } from './index'
import { ShaniuBrandName } from './BrandName'
import { SHANIU_BRAND_NAME } from './brand-name'

/** 取出某个选择器的声明块（先剥注释，避免注释里的冒号污染键名）。 */
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

/** 记录插件对槽位注册表做了什么。 */
interface SlotsProbe {
  waitedKeys: string[]
  registrations: { name: string; component: unknown }[]
  /** 交给上下文的假 slots 服务。 */
  slots: {
    inject(key: string, callback: () => unknown): () => void
    register(options: { name: string }, component: unknown): () => void
  }
  /** 模拟宿主声明槽位：触发插件等待中的回调。 */
  declare(key: string): void
}

function useSlotsProbe(): SlotsProbe {
  const waiters = new Map<string, (() => unknown)[]>()
  const probe: SlotsProbe = {
    waitedKeys: [],
    registrations: [],
    slots: {
      inject: (key: string, callback: () => unknown) => {
        probe.waitedKeys.push(key)
        waiters.set(key, [...(waiters.get(key) ?? []), callback])
        return () => {}
      },
      register: (options: { name: string }, component: unknown) => {
        probe.registrations.push({ name: options.name, component })
        return () => {}
      },
    },
    // 模拟宿主声明槽位：触发插件等待中的回调
    declare: (key: string): void => {
      for (const callback of waiters.get(key) ?? []) callback()
    },
  }
  return probe
}

describe('src/brand：侧栏品牌名占用者', () => {
  let ctx: Context
  let probe: SlotsProbe

  beforeEach(() => {
    ctx = new Context()
    probe = useSlotsProbe()
    ctx.provide('slots', probe.slots as never)
  })

  it('插件只依赖 slots 服务', () => {
    expect(brandInject).toEqual(['slots'])
  })

  it('挂载时等的是 sidebar.brand.name，且此时不注册任何东西', () => {
    brandApply(ctx)
    expect(probe.waitedKeys).toEqual(['sidebar.brand.name'])
    expect(probe.registrations).toHaveLength(0)
  })

  it('槽位就位后填入 ShaniuBrandName，槽名与声明键一致', () => {
    brandApply(ctx)
    probe.declare('sidebar.brand.name')
    expect(probe.registrations).toHaveLength(1)
    expect(probe.registrations[0]).toEqual({ name: 'sidebar.brand.name', component: ShaniuBrandName })
  })

  it('占用者渲染 SHANIU，且不带上游本地构建字样', () => {
    render(<ShaniuBrandName />)
    expect(screen.getByText(SHANIU_BRAND_NAME)).toBeTruthy()
    expect(document.body.textContent).not.toContain('本地构建')
  })

  it('品牌名常量与构建期标题同源', () => {
    expect(SHANIU_BRAND_NAME).toBe('SHANIU')
  })

  it('排版规格与上游 `.brandName` 逐项一致（不复制上游 SVG，只对齐字重/字号/字距/尺寸）', () => {
    const own = declarations(
      readFileSync(join(import.meta.dirname, 'brand-name.css'), 'utf8'),
      '.shaniu-brand-name',
    )
    const upstream = declarations(
      readFileSync(
        join(
          import.meta.dirname, '..', 'dsh', 'packages', 'client', 'ui-sidebar',
          'src', 'client', 'SidebarRoot.module.css',
        ),
        'utf8',
      ),
      '.brandName',
    )
    for (const key of ['height', 'font-size', 'font-weight', 'line-height', 'letter-spacing']) {
      expect(own.get(key), `declaration ${key}`).toBe(upstream.get(key))
    }
  })
})
