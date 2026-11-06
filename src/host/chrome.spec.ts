/**
 * 控制层的行为契约：每个控件对应哪个窗口动作，以及窗口状态怎么回流到 DOM。
 * 断言对象是「点了会不会调错 API / 该不调的时候有没有乱调」，不是图标或类名长什么样。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mountWindowChrome, TITLEBAR_HEIGHT, type ResizeEdge, type WindowControls } from './chrome'

const HEIGHT_VAR = '--dsh-windows-titlebar-height'
const EDGES: readonly ResizeEdge[] = [
  'North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest',
]

function fakeControls(maximized = false): WindowControls & { calls: string[] } {
  const calls: string[] = []
  const record = (name: string) => () => {
    calls.push(name)
    return Promise.resolve()
  }
  return {
    calls,
    minimize: record('minimize'),
    toggleMaximize: record('toggleMaximize'),
    close: record('close'),
    startDragging: record('startDragging'),
    startResizeDragging: (edge: ResizeEdge) => {
      calls.push(`resize:${edge}`)
      return Promise.resolve()
    },
    isMaximized: () => Promise.resolve(maximized),
  }
}

/** Promise 链落地：回查最大化状态走的是异步 IPC。 */
const flush = (): Promise<void> => new Promise(resolve => { setTimeout(resolve, 0) })

function find(selector: string): HTMLElement {
  const el = document.querySelector<HTMLElement>(selector)
  if (el === null) throw new Error(`控件缺失：${selector}`)
  return el
}

const buttonOf = (name: string): HTMLElement => find(`.sn-caption-btn[data-test="${name}"]`)
const handleOf = (edge: ResizeEdge): HTMLElement => find(`.sn-resize[data-edge="${edge}"]`)

/** 按下事件：拖拽走 mousedown，缩放走 pointerdown；detail 是 Tauri 判定双击的依据。 */
function press(el: HTMLElement, type: 'mousedown' | 'pointerdown', detail = 1, button = 0): void {
  el.dispatchEvent(new MouseEvent(type, { bubbles: true, cancelable: true, button, detail }))
}

/** 造一行 DSH 自己标了拖拽的 chrome row，行内同时放一个普通 span 和一个按钮。 */
function dragRow(): { blank: HTMLElement; action: HTMLElement } {
  const row = document.createElement('div')
  row.setAttribute('data-window-drag', '')
  const blank = document.createElement('span')
  const action = document.createElement('button')
  action.type = 'button'
  row.append(blank, action)
  document.body.append(row)
  return { blank, action }
}

describe('window chrome：控件 → 窗口动作', () => {
  /** 每个用例装上的控制层都要退掉：留着会互相抢着写 html 上的状态属性。 */
  const disposers: Array<() => void> = []
  const mount = (controls: WindowControls): (() => void) => {
    const dispose = mountWindowChrome(controls)
    disposers.push(dispose)
    return dispose
  }

  beforeEach(() => {
    document.body.innerHTML = ''
    const html = document.documentElement
    for (const attr of ['data-windows-titlebar', 'data-maximized']) html.removeAttribute(attr)
    html.style.removeProperty(HEIGHT_VAR)
  })

  afterEach(() => {
    for (const dispose of disposers.splice(0)) dispose()
  })

  it('安装时打开 DSH 的 Windows 版式开关并给出高度，卸载时全部收回', () => {
    const dispose = mount(fakeControls())
    const html = document.documentElement
    expect(html.hasAttribute('data-windows-titlebar')).toBe(true)
    expect(html.style.getPropertyValue(HEIGHT_VAR)).toBe(TITLEBAR_HEIGHT)
    expect(document.querySelectorAll('.sn-caption-btn')).toHaveLength(3)
    expect(document.querySelectorAll('.sn-resize')).toHaveLength(EDGES.length)

    dispose()
    expect(html.hasAttribute('data-windows-titlebar')).toBe(false)
    expect(html.style.getPropertyValue(HEIGHT_VAR)).toBe('')
    expect(document.querySelector('.sn-chrome')).toBeNull()
  })

  it('三颗标题按钮各自只调对应的窗口动作', () => {
    const controls = fakeControls()
    mount(controls)
    buttonOf('minimize').click()
    buttonOf('maximize').click()
    buttonOf('close').click()
    expect(controls.calls).toEqual(['minimize', 'toggleMaximize', 'close'])
  })

  it('标题按钮的点击不会被当成拖拽起手', () => {
    const controls = fakeControls()
    mount(controls)
    press(find('.sn-close'), 'mousedown')
    press(find('.sn-caption'), 'mousedown')
    expect(controls.calls).toEqual([])
  })

  it('八个缩放手柄各自把方向交给系统命中测试', () => {
    const controls = fakeControls()
    mount(controls)
    for (const edge of EDGES) press(handleOf(edge), 'pointerdown')
    expect(controls.calls).toEqual(EDGES.map(edge => `resize:${edge}`))
  })

  it('拖拽行本身与行内空白都拖得动窗口', () => {
    const controls = fakeControls()
    mount(controls)
    const { blank } = dragRow()
    press(find('[data-window-drag]'), 'mousedown')
    press(blank, 'mousedown')
    expect(controls.calls).toEqual(['startDragging', 'startDragging'])
  })

  it('拖拽行内的按钮不连带拖窗口', () => {
    const controls = fakeControls()
    mount(controls)
    press(dragRow().action, 'mousedown')
    expect(controls.calls).toEqual([])
  })

  it('非左键与三击以上都不动窗口', () => {
    const controls = fakeControls()
    mount(controls)
    const { blank } = dragRow()
    press(blank, 'mousedown', 1, 2)
    press(blank, 'mousedown', 3)
    expect(controls.calls).toEqual([])
  })

  it('拖拽行上的双击切最大化而不是再次拖拽', () => {
    const controls = fakeControls()
    mount(controls)
    press(dragRow().blank, 'mousedown', 2)
    expect(controls.calls).toEqual(['toggleMaximize'])
  })

  it('卸载后拖拽行不再产生任何窗口动作', () => {
    const controls = fakeControls()
    const dispose = mount(controls)
    const { blank } = dragRow()
    dispose()
    press(blank, 'mousedown')
    expect(controls.calls).toEqual([])
  })

  it('最大化状态来自窗口回查：初值与尺寸变化后都同步属性和按钮文案', async () => {
    const html = document.documentElement
    const controls = fakeControls(false)
    mount(controls)
    await flush()
    expect(html.hasAttribute('data-maximized')).toBe(false)
    expect(buttonOf('maximize').getAttribute('aria-label')).toBe('最大化')

    // 窗口被系统最大化后，尺寸变化事件驱动一次回查
    controls.isMaximized = vi.fn(() => Promise.resolve(true))
    window.dispatchEvent(new Event('resize'))
    await flush()
    expect(controls.isMaximized).toHaveBeenCalledTimes(1)
    expect(html.hasAttribute('data-maximized')).toBe(true)
    expect(buttonOf('maximize').getAttribute('aria-label')).toBe('还原')
  })

  it('晚到的旧回查不覆盖新一次的结果', async () => {
    const html = document.documentElement
    const controls = fakeControls(false)
    const gates: Array<(maximized: boolean) => void> = []
    controls.isMaximized = () => new Promise<boolean>(resolve => { gates.push(resolve) })
    mount(controls)
    window.dispatchEvent(new Event('resize'))
    expect(gates).toHaveLength(2)

    gates[1](true)
    await flush()
    expect(html.hasAttribute('data-maximized')).toBe(true)

    gates[0](false)
    await flush()
    expect(html.hasAttribute('data-maximized')).toBe(true)
  })

  it('回查失败只让图标停在旧档，不抛给用户', async () => {
    const html = document.documentElement
    const controls = fakeControls(true)
    controls.isMaximized = () => Promise.reject(new Error('窗口正在销毁'))
    mount(controls)
    await flush()
    expect(html.hasAttribute('data-maximized')).toBe(false)
  })
})
