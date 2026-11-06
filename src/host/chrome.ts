/**
 * 无边框窗口的宿主控制层：三颗标题按钮（最小化 / 最大化-还原 / 关闭）、八向缩放手柄，
 * 以及把 DSH 已有的拖拽行接成窗口移动。
 *
 * 本文件只做 DOM 编排，窗口动作一律经 WindowControls 注入，所以「点哪个控件 → 调哪个
 * 窗口 API」在 jsdom 下即可断言；真实的 Tauri 适配在 tauri-chrome.ts。
 *
 * 两处不是随意选择：
 * - 拖拽不用 DSH 上游的 -webkit-app-region，那是 Electron 专有特性，Tauri 的 WebView2
 *   不认。也不用 data-tauri-drag-region：它的裸值语义要求按下必须落在标记元素本身，而
 *   DSH 顶部条的几何由 .frame::before 伪元素画、行内又套满 span，按下命中的是子节点。
 *   改为监听 DSH 已经标好的 [data-window-drag] 行（SidebarRoot 的 logoRow、
 *   ConversationHeader 等），由我们自己向上判定 —— 语义等同它的 "deep" 档。
 * - 标题按钮与手柄是覆盖层，但只占顶部右侧一角与窗口外沿，不再横跨整条顶部带：
 *   Windows 版式下 DSH 会把侧栏控件摆进那条带里（SidebarRoot.module.css 按
 *   --dsh-windows-titlebar-height 定位），整条压上去会吞掉它们的点击。
 */

/** Tauri startResizeDragging 的八个方向。 */
export type ResizeEdge =
  | 'North' | 'South' | 'East' | 'West'
  | 'NorthEast' | 'NorthWest' | 'SouthEast' | 'SouthWest'

/** 控制层需要的窗口动作面。 */
export interface WindowControls {
  minimize(): Promise<void>
  toggleMaximize(): Promise<void>
  close(): Promise<void>
  startDragging(): Promise<void>
  startResizeDragging(edge: ResizeEdge): Promise<void>
  isMaximized(): Promise<boolean>
}

/** 标题条高度，同时写进 DSH 的 --dsh-windows-titlebar-height 让 AppFrame 让位。 */
export const TITLEBAR_HEIGHT = '40px'

const MAXIMIZED_ATTR = 'data-maximized'
const TITLEBAR_ATTR = 'data-windows-titlebar'
const HEIGHT_VAR = '--dsh-windows-titlebar-height'
const DRAG_ROW_SELECTOR = '[data-window-drag]'
const SVG_NS = 'http://www.w3.org/2000/svg'

const RESIZE_EDGES: readonly ResizeEdge[] = [
  'North', 'South', 'East', 'West', 'NorthEast', 'NorthWest', 'SouthEast', 'SouthWest',
]

/**
 * 命中即不拖拽的可交互元素，取自 Tauri 自带的 drag.js（CLICKABLE_TAGS / INTERACTIVE_ROLES）：
 * 抄同一份判据，行内按钮才会既点得动又不连带拖动窗口。
 */
const CLICKABLE_TAGS = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA', 'LABEL', 'SUMMARY'])
const INTERACTIVE_ROLES = new Set([
  'button', 'link', 'menuitem', 'tab', 'checkbox', 'radio', 'switch', 'option',
])

function blocksDrag(el: Element): boolean {
  if (CLICKABLE_TAGS.has(el.tagName)) return true
  if (INTERACTIVE_ROLES.has(el.getAttribute('role') ?? '')) return true
  const editable = el.getAttribute('contenteditable')
  if (editable !== null && editable !== 'false') return true
  const tabIndex = el.getAttribute('tabindex')
  return tabIndex !== null && tabIndex !== '-1'
}

/** 一条 16x16 描边图标；形状统一用 path，省掉 rect/path 两套构造。 */
function icon(...paths: string[]): SVGElement {
  const svg = document.createElementNS(SVG_NS, 'svg')
  svg.setAttribute('viewBox', '0 0 16 16')
  svg.setAttribute('aria-hidden', 'true')
  svg.setAttribute('focusable', 'false')
  const g = document.createElementNS(SVG_NS, 'g')
  g.setAttribute('fill', 'none')
  g.setAttribute('stroke', 'currentColor')
  g.setAttribute('stroke-width', '1.1')
  for (const d of paths) {
    const path = document.createElementNS(SVG_NS, 'path')
    path.setAttribute('d', d)
    g.append(path)
  }
  svg.append(g)
  return svg
}

function button(name: string, label: string, ...children: Element[]): HTMLElement {
  const btn = document.createElement('button')
  btn.type = 'button'
  btn.className = 'sn-caption-btn'
  btn.dataset.test = name
  btn.setAttribute('aria-label', label)
  btn.title = label
  btn.append(...children)
  return btn
}

/**
 * 装配控制层，返回卸载函数。
 *
 * 顺带打开 DSH 的 Windows 版式开关：AppFrame / DockKit / SidebarRoot 的样式全部按
 * html[data-windows-titlebar] 分支让出顶部条，宿主必须同时给出高度，否则内容顶到标题条底下。
 */
export function mountWindowChrome(controls: WindowControls): () => void {
  const root = document.createElement('div')
  root.className = 'sn-chrome'

  const minimize = button('minimize', '最小化', icon('M3.5 8h9'))
  const restoreGlyph = document.createElement('span')
  restoreGlyph.className = 'sn-glyph-restore'
  restoreGlyph.append(icon('M5 3h8v8h-8z', 'M3 6v7h7'))
  const maximize = button('maximize', '最大化', icon('M3.5 3.5h9v9h-9z'), restoreGlyph)
  const close = button('close', '关闭', icon('M3.5 3.5l9 9', 'M12.5 3.5l-9 9'))
  close.classList.add('sn-close')
  const caption = document.createElement('div')
  caption.className = 'sn-caption'
  caption.append(minimize, maximize, close)
  root.append(caption)

  for (const edge of RESIZE_EDGES) {
    const handle = document.createElement('div')
    handle.className = `sn-resize sn-resize-${edge.toLowerCase()}`
    handle.dataset.edge = edge
    root.append(handle)
  }
  document.body.append(root)

  const html = document.documentElement
  html.setAttribute(TITLEBAR_ATTR, '')
  html.style.setProperty(HEIGHT_VAR, TITLEBAR_HEIGHT)

  let disposed = false
  let seq = 0

  function applyMaximized(maximized: boolean): void {
    html.toggleAttribute(MAXIMIZED_ATTR, maximized)
    const label = maximized ? '还原' : '最大化'
    maximize.setAttribute('aria-label', label)
    maximize.title = label
  }

  /**
   * 尺寸变化后回查最大化状态。call 序号丢掉乱序返回的旧结果；IPC 失败（窗口正在销毁）
   * 只让图标停在旧档，不打断用户。
   */
  function syncMaximized(): void {
    const call = ++seq
    controls.isMaximized().then(maximized => {
      if (!disposed && call === seq) applyMaximized(maximized)
    }, () => {})
  }

  function act(action: () => Promise<void>): void {
    void action()
  }

  root.addEventListener('click', event => {
    const target = event.target
    if (!(target instanceof Element)) return
    switch (target.closest<HTMLElement>('.sn-caption-btn')?.dataset.test) {
      case 'minimize': act(() => controls.minimize()); break
      case 'maximize': act(() => controls.toggleMaximize()); break
      case 'close': act(() => controls.close()); break
      default: break
    }
  })

  // 按下即把命中测试交回系统；preventDefault 只为压掉文本选中光标
  root.addEventListener('pointerdown', event => {
    const target = event.target
    if (!(target instanceof Element)) return
    const handle = target.closest<HTMLElement>('.sn-resize')
    if (handle === null) return
    event.preventDefault()
    act(() => controls.startResizeDragging(handle.dataset.edge as ResizeEdge))
  })

  /**
   * 从按下点向上找第一个声明拖拽的行；途中撞上可交互元素就放弃，
   * 这样行内按钮既点得动，也不会连带把窗口拖走。
   */
  function onButtonDown(event: MouseEvent): void {
    if (event.button !== 0) return
    // detail 1 = 起手拖拽，2 = 双击切换最大化；拖拽中重复触发没有意义
    if (event.detail !== 1 && event.detail !== 2) return
    let el = event.target instanceof Element ? event.target : null
    let draggable = false
    for (; el !== null; el = el.parentElement) {
      if (blocksDrag(el)) return
      if (el.matches(DRAG_ROW_SELECTOR)) {
        draggable = true
        break
      }
      if (el === root) return // 覆盖层自带判定，不走 app 内容的拖拽行
    }
    if (!draggable) return
    event.preventDefault()
    act(event.detail === 2 ? () => controls.toggleMaximize() : () => controls.startDragging())
  }

  document.addEventListener('mousedown', onButtonDown)
  window.addEventListener('resize', syncMaximized)
  syncMaximized()

  return () => {
    disposed = true
    document.removeEventListener('mousedown', onButtonDown)
    window.removeEventListener('resize', syncMaximized)
    root.remove()
    html.removeAttribute(TITLEBAR_ATTR)
    html.removeAttribute(MAXIMIZED_ATTR)
    html.style.removeProperty(HEIGHT_VAR)
  }
}
