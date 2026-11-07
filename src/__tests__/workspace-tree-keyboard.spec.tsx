/**
 * SH-101：侧栏工作区树的 ARIA 键盘模型。
 *
 * 为什么这条测试在 SHANIU 面而不是上游 `src/dsh/**` 的 spec 里：上游用例依赖
 * `@deepseek-ai/dsh-client-test-runtime`，该包未随 vendored 树引入（根 vitest 也把
 * `src/dsh/**` 排除在验收面外），放在那里等于永远不跑。这里按既有先例
 * （`src/__tests__/streaming.test.tsx`）直接用相对路径引用 vendored 源码。
 *
 * 修复的对象是行为而不是崩溃本身：Chromium 对 `role="treeitem"` 的 UIA
 * ExpandCollapsePattern 会 focus 行并合成 ArrowLeft/ArrowRight，而方向键的默认动作
 * 在侧栏滚动容器里会让 WebView2 渲染进程死锁（真机取证见 SH-101 报告）。因此这里断言
 * 两件事：树键盘模型本身（焦点移动 / 展开折叠 / 无关按键放行），以及这些键全部被
 * preventDefault（防默认动作，即崩溃路径）。
 */
// @vitest-environment jsdom
import { act, fireEvent, render } from '@testing-library/react'
import { useState } from 'react'
import { describe, expect, it, vi } from 'vitest'
import { AnimatedRows } from '../dsh/packages/client/ui-primitives/src/AnimatedRows.tsx'
import { handleTreeKeyDown } from '../dsh/packages/client/ui-workspace/src/client/rows/treeKeyboard.ts'

/**
 * Group toggles are applied one task after the keydown (synchronous toggles deadlock the
 * WebView2 renderer under Chromium's UIA-driven key dispatch — SH-101), so assertions wait
 * for that task.
 */
const settle = () => act(async () => { await new Promise(resolve => { setTimeout(resolve, 0) }) })

/** One expanded group with a child, plus a second collapsed group. */
function Tree() {
  const [expanded, setExpanded] = useState(true)
  return (
    <div role="tree" onKeyDown={handleTreeKeyDown}>
      <div id="alpha" role="treeitem" tabIndex={0} aria-expanded={expanded} onClick={() => { setExpanded(value => !value) }}>
        Alpha
      </div>
      {expanded && <div id="child" role="treeitem" tabIndex={0}>session-one</div>}
      <div id="beta" role="treeitem" tabIndex={0} aria-expanded="false" onClick={() => {}}>Beta</div>
    </div>
  )
}

function mountTree() {
  const { container } = render(<Tree />)
  const row = (id: string) => container.querySelector<HTMLElement>(`#${id}`)!
  return { container, row }
}

describe('workspace tree keyboard model', () => {
  it('moves focus with Down, Up, Home and End, and prevents each of them', () => {
    const { row } = mountTree()
    row('alpha').focus()
    expect(fireEvent.keyDown(row('alpha'), { key: 'ArrowDown' })).toBe(false)
    expect(document.activeElement).toBe(row('child'))
    expect(fireEvent.keyDown(row('child'), { key: 'ArrowUp' })).toBe(false)
    expect(document.activeElement).toBe(row('alpha'))
    expect(fireEvent.keyDown(row('alpha'), { key: 'End' })).toBe(false)
    expect(document.activeElement).toBe(row('beta'))
    expect(fireEvent.keyDown(row('beta'), { key: 'Home' })).toBe(false)
    expect(document.activeElement).toBe(row('alpha'))
  })

  it('collapses an expanded group with Left and re-expands it with Right', async () => {
    const { row } = mountTree()
    expect(fireEvent.keyDown(row('alpha'), { key: 'ArrowLeft' })).toBe(false)
    await settle()
    expect(row('alpha').getAttribute('aria-expanded')).toBe('false')
    expect(row('child')).toBeNull()
    expect(fireEvent.keyDown(row('alpha'), { key: 'ArrowRight' })).toBe(false)
    await settle()
    expect(row('alpha').getAttribute('aria-expanded')).toBe('true')
    expect(row('child')).not.toBeNull()
  })

  it('enters a group with Right and returns to its header with Left on a child row', () => {
    const { row } = mountTree()
    row('alpha').focus()
    expect(fireEvent.keyDown(row('alpha'), { key: 'ArrowRight' })).toBe(false)
    expect(document.activeElement).toBe(row('child'))
    expect(fireEvent.keyDown(row('child'), { key: 'ArrowLeft' })).toBe(false)
    expect(document.activeElement).toBe(row('alpha'))
  })

  it('toggles a group with Enter and leaves unrelated keys to the page', async () => {
    const { row } = mountTree()
    expect(fireEvent.keyDown(row('alpha'), { key: 'a' })).toBe(true)
    expect(row('alpha').getAttribute('aria-expanded')).toBe('true')
    expect(fireEvent.keyDown(row('alpha'), { key: 'Enter' })).toBe(false)
    await settle()
    expect(row('alpha').getAttribute('aria-expanded')).toBe('false')
  })

  it('ignores modified keys and keydowns outside a treeitem', () => {
    const { container, row } = mountTree()
    row('alpha').focus()
    expect(fireEvent.keyDown(row('alpha'), { key: 'ArrowDown', ctrlKey: true })).toBe(true)
    expect(document.activeElement).toBe(row('alpha'))
    expect(fireEvent.keyDown(container.querySelector('[role="tree"]')!, { key: 'ArrowDown' })).toBe(true)
  })
})

describe('AnimatedRows', () => {
  it('forwards list keydowns to the owner that supplies the rows', () => {
    const onKeyDown = vi.fn()
    const { getByText } = render(
      <AnimatedRows className="rows" role="tree" label="工作区" rowKeys={['alpha']} ready resetKey="" onKeyDown={onKeyDown}>
        <div role="treeitem" tabIndex={0} aria-expanded="true">Alpha</div>
      </AnimatedRows>,
    )
    fireEvent.keyDown(getByText('Alpha'), { key: 'ArrowLeft' })
    expect(onKeyDown).toHaveBeenCalledOnce()
    expect(onKeyDown.mock.calls[0]![0].key).toBe('ArrowLeft')
  })
})
