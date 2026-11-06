// @vitest-environment jsdom
/**
 * 自有品牌插件的接缝契约测试。
 *
 * 这里不复建 SlotRegistry 的声明引导链（'root' 由谁声明属于 vendored 内部装配，
 * 复刻它只会让测试跟着上游私有实现走）。测的是本插件自己承诺的两件事：
 * 它等的是 `sidebar.brand.name` 这个键，以及槽位就位时填进去的组件就是 ShaniuBrandName。
 * 端到端是否真的换掉 "DSH 本地构建"，由桌面窗口截图验收（见交付说明）。
 */
import { beforeEach, describe, expect, it } from 'vitest'
import { render, screen } from '@testing-library/react'
import { Context } from '@deepseek-ai/cordis'
import { apply as brandApply, inject as brandInject } from './index'
import { ShaniuBrandName } from './BrandName'
import { SHANIU_BRAND_NAME } from './brand-name'

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
})
