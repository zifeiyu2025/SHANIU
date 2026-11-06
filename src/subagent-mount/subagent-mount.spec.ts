/**
 * ui-subagent 挂载冒烟（SH-109）：移植包在 SHANIU 壳的装配面上能完成全部注册。
 * 判据是"注册了哪些槽位/资源"（装配契约），不是渲染细节——渲染归上游包的测试，
 * 这里的职责是守住"SHANIU 壳提供的服务面够它挂载"这条接线，防止后续的装配改动
 * 悄悄拆掉泳道入口。
 */
import { describe, expect, it, vi } from 'vitest'
import { apply } from '../dsh/packages/client/ui-subagent/src/client/index.ts'

interface Registration {
  readonly slot: string
  readonly options: Record<string, unknown>
}

/** 记录注册结果的最小 ctx；只声明 apply() 与其回调真正触碰的成员。 */
function harness(): { ctx: Record<string, unknown>; slots: Registration[]; counts: { resources: number; tabs: number } } {
  const slots: Registration[] = []
  // 计数走对象引用：注册回调在 apply()（测试体内）才执行，按值解构会读到快照 0
  const counts = { resources: 0, tabs: 0 }
  const scope = {
    effect: (fn: () => unknown) => fn(),
    resources: { register: () => { counts.resources += 1; return () => {} } },
    sidebarRightTabs: { register: () => { counts.tabs += 1; return () => {} } },
    slots: {
      inject: (_slotName: string, callback: () => () => void) => { callback(); return () => {} },
      register: (options: Record<string, unknown>, _component: unknown) => { void options; return () => {} },
    },
    sessions: { list: { getSnapshot: () => ({ byId: {} }) } },
  }
  const ctx = {
    effect: (fn: () => unknown) => fn(),
    locale: { register: () => () => {}, bind: () => (key: string) => key },
    inject: (names: readonly string[], callback: (scope: unknown) => unknown) => {
      void names
      callback(scope)
      return () => {}
    },
    slots: {
      inject: (slotName: string, callback: () => () => void) => { void slotName; callback(); return () => {} },
      register: (options: Record<string, unknown>, _component: unknown) => {
        slots.push({ slot: String(options['name']), options })
        return () => {}
      },
      entries: () => [],
      subscribe: () => () => {},
    },
    uiWorkspace: { openSession: vi.fn() },
    sidebarRight: { openResource: vi.fn() },
    sessions: { refreshProjections: vi.fn() },
  }
  return { ctx, slots, counts }
}

describe('ui-subagent 挂载（SH-109）', () => {
  it('apply() 注册泳道的四个座位：面包屑 lineage、头部目录、只读 composer、侧栏聊天资源', () => {
    const { ctx, slots, counts } = harness()
    apply(ctx as never)

    const slotNames = slots.map(entry => entry.slot)
    expect(slotNames).toContain('conversation.session.header.lineage')
    expect(slotNames).toContain('conversation.session.header.actions')
    expect(slotNames).toContain('conversation.composer')
    // 目录动作是头部 actions 的一个具名条目（id=subagent-catalog，order 10）
    const catalog = slots.find(entry => entry.options['id'] === 'subagent-catalog')
    expect(catalog).toBeDefined()
    // 子会话旁路聊天：资源 + 右栏页签都注册
    expect(counts.resources).toBe(1)
    expect(counts.tabs).toBe(1)
  })
})
