/**
 * 输入框右键菜单（宿主自绘，开发与发布一致）。
 *
 * 桌面发布版没有可用的 WebView2 默认菜单（dev 窗口实测右键输入框无反应），
 * 这里为所有可编辑控件（input / textarea / contenteditable）补一套标准编辑菜单：
 * 撤销、重做、剪切、复制、粘贴、全选。
 *
 * 行为约定：
 * - 只在可编辑控件上接管 `contextmenu`，其余目标不干预（DSH 自己的对象右键菜单
 *   仍按原逻辑走）。
 * - 打开时先计算各命令可用性：剪切/复制看选区，撤销/重做交给 `queryCommandEnabled`，
 *   粘贴恒可用（真正读取失败时静默结束）。
 * - 执行前把焦点交还控件（右键可能没有聚焦），命令走 `execCommand`：native 输入框
 *   与 Lexical 的 contenteditable 都会收到对应的 beforeinput（historyUndo / insertText）。
 * - 粘贴文本来自宿主剪贴板桥（见 host/clipboard.ts）；`insertText` 被拒时对
 *   input/textarea 走 setRangeText 兜底。
 */
import { readClipboardText, writeClipboardText } from './clipboard'
import { hostT, type HostTextKey } from './host-locale'
import './input-menu.css'

/** 可执行的标准编辑命令。 */
type MenuAction = 'undo' | 'redo' | 'cut' | 'copy' | 'paste' | 'selectAll'

interface MenuItem {
  readonly action: MenuAction
  readonly label: HostTextKey
  readonly shortcut: string
}

/** 菜单分组（组间画分隔线）。文案在打开时按当前界面语言取（`hostT`）。 */
const MENU_GROUPS: readonly (readonly MenuItem[])[] = [
  [
    { action: 'undo', label: 'contextMenu.undo', shortcut: 'Ctrl+Z' },
    { action: 'redo', label: 'contextMenu.redo', shortcut: 'Ctrl+Y' },
  ],
  [
    { action: 'cut', label: 'contextMenu.cut', shortcut: 'Ctrl+X' },
    { action: 'copy', label: 'contextMenu.copy', shortcut: 'Ctrl+C' },
    { action: 'paste', label: 'contextMenu.paste', shortcut: 'Ctrl+V' },
  ],
  [
    { action: 'selectAll', label: 'contextMenu.selectAll', shortcut: 'Ctrl+A' },
  ],
]

/** 文本型 input 才接管；勾选/颜色/日期等控件保留各自的原生交互。 */
const TEXT_INPUT_TYPES = new Set(['', 'text', 'search', 'url', 'email', 'password', 'number', 'tel'])

/** 当前打开的菜单与其目标控件；同一时刻只允许一个。 */
let openMenu: HTMLElement | null = null
let openTarget: HTMLElement | null = null
let disposeOpen: (() => void) | null = null

/** 找到事件目标所属的可编辑控件。 */
function editableTarget(target: EventTarget | null): HTMLElement | undefined {
  if (!(target instanceof Element)) return undefined
  const element = target.closest<HTMLElement>('input, textarea, [contenteditable]')
  if (element === null) return undefined
  if (element instanceof HTMLTextAreaElement) return element
  if (element instanceof HTMLInputElement) return TEXT_INPUT_TYPES.has(element.type) ? element : undefined
  return element.isContentEditable ? element : undefined
}

/** 控件内是否存在非空选区。 */
function hasSelection(editable: HTMLElement): boolean {
  if (editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement) {
    return (editable.selectionEnd ?? 0) > (editable.selectionStart ?? 0)
  }
  const selection = window.getSelection()
  if (selection === null || selection.isCollapsed) return false
  return (selection.anchorNode !== null && editable.contains(selection.anchorNode))
    || (selection.focusNode !== null && editable.contains(selection.focusNode))
}

/** 控件是否还有内容（全选的可用性判据）。 */
function hasContent(editable: HTMLElement): boolean {
  if (editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement) {
    return editable.value.length > 0
  }
  return (editable.textContent ?? '').length > 0
}

/** 查询文档级编辑命令是否可用；jsdom 没有该 API 时按可用处理。 */
function commandEnabled(command: 'undo' | 'redo'): boolean {
  const query = (document as Document & { queryCommandEnabled?: (cmd: string) => boolean }).queryCommandEnabled
  return typeof query === 'function' ? query.call(document, command) : true
}

/** 执行一条编辑命令；环境不支持时返回 false。 */
function runEditCommand(command: string, value?: string): boolean {
  const run = (document as Document & { execCommand?: (cmd: string, ui?: boolean, val?: string) => boolean }).execCommand
  if (typeof run !== 'function') return false
  try {
    return run.call(document, command, false, value)
  } catch {
    return false
  }
}

/** 全选控件内容。 */
function selectAll(editable: HTMLElement): void {
  if (editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement) {
    editable.select()
    return
  }
  const range = document.createRange()
  range.selectNodeContents(editable)
  const selection = window.getSelection()
  selection?.removeAllRanges()
  selection?.addRange(range)
}

/** 把剪贴板文本粘进控件：优先 insertText（Lexical 走 beforeinput），否则原生选区替换。 */
async function pasteInto(editable: HTMLElement): Promise<void> {
  let text = ''
  try {
    text = await readClipboardText()
  } catch {
    return
  }
  if (text.length === 0) return
  if (runEditCommand('insertText', text)) return
  if (editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement) {
    const start = editable.selectionStart ?? editable.value.length
    const end = editable.selectionEnd ?? start
    editable.setRangeText(text, start, end, 'end')
    editable.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: text }))
  }
}

/** 复制/剪切走到系统剪贴板；execCommand 失败时用剪贴板桥兜底（仅复制）。 */
async function copyOut(editable: HTMLElement, cut: boolean): Promise<void> {
  if (cut ? runEditCommand('cut') : runEditCommand('copy')) return
  const selected = editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement
    ? editable.value.slice(editable.selectionStart ?? 0, editable.selectionEnd ?? 0)
    : window.getSelection()?.toString() ?? ''
  if (selected.length === 0) return
  try {
    await writeClipboardText(selected)
  } catch {
    return
  }
  if (cut) {
    if (editable instanceof HTMLInputElement || editable instanceof HTMLTextAreaElement) {
      const start = editable.selectionStart ?? 0
      const end = editable.selectionEnd ?? start
      editable.setRangeText('', start, end, 'end')
      editable.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'deleteContentBackward' }))
    } else {
      runEditCommand('delete')
    }
  }
}

/** 执行菜单动作。 */
function perform(action: MenuAction, editable: HTMLElement): void {
  if (document.activeElement !== editable) editable.focus({ preventScroll: true })
  switch (action) {
    case 'undo':
      runEditCommand('undo')
      break
    case 'redo':
      runEditCommand('redo')
      break
    case 'cut':
      void copyOut(editable, true)
      break
    case 'copy':
      void copyOut(editable, false)
      break
    case 'paste':
      void pasteInto(editable)
      break
    case 'selectAll':
      selectAll(editable)
      break
  }
}

/** 关闭当前菜单（幂等）。 */
export function closeInputContextMenu(): void {
  if (disposeOpen !== null) disposeOpen()
  openMenu?.remove()
  openMenu = null
  openTarget = null
  disposeOpen = null
}

/**
 * 在指针处打开菜单。
 * @param editable - 目标可编辑控件。
 * @param x - 视口坐标（clientX）。
 * @param y - 视口坐标（clientY）。
 */
function showMenu(editable: HTMLElement, x: number, y: number): void {
  closeInputContextMenu()
  const menu = document.createElement('div')
  menu.className = 'sn-input-menu'
  menu.setAttribute('role', 'menu')
  menu.setAttribute('aria-label', hostT('contextMenu.label'))
  // 与 DSH `Menu.module.css` 同构：卡片只负责表面，滚动发生在内层 viewport。
  const viewport = document.createElement('div')
  viewport.className = 'sn-input-menu-viewport'

  const selection = hasSelection(editable)
  const enabled: Record<MenuAction, boolean> = {
    undo: commandEnabled('undo'),
    redo: commandEnabled('redo'),
    cut: selection,
    copy: selection,
    paste: true,
    selectAll: hasContent(editable),
  }

  for (const [index, group] of MENU_GROUPS.entries()) {
    if (index > 0) {
      const separator = document.createElement('div')
      separator.className = 'sn-input-menu-sep'
      separator.setAttribute('role', 'separator')
      viewport.append(separator)
    }
    for (const item of group) {
      const button = document.createElement('button')
      button.type = 'button'
      button.className = 'sn-input-menu-item'
      button.setAttribute('role', 'menuitem')
      button.disabled = !enabled[item.action]
      const label = document.createElement('span')
      label.textContent = hostT(item.label)
      const shortcut = document.createElement('span')
      shortcut.className = 'sn-input-menu-shortcut'
      shortcut.textContent = item.shortcut
      button.append(label, shortcut)
      button.addEventListener('click', () => {
        closeInputContextMenu()
        perform(item.action, editable)
      })
      viewport.append(button)
    }
  }
  menu.append(viewport)

  menu.style.left = '0px'
  menu.style.top = '0px'
  document.body.append(menu)
  // 先量再夹：保证菜单始终完整落在视口内
  const rect = menu.getBoundingClientRect()
  menu.style.left = `${Math.max(4, Math.min(x, window.innerWidth - rect.width - 4))}px`
  menu.style.top = `${Math.max(4, Math.min(y, window.innerHeight - rect.height - 4))}px`

  openMenu = menu
  openTarget = editable

  /** 当前可用的菜单项（键盘导航只看可用项，跳过禁用项）。 */
  const enabledItems = (): HTMLButtonElement[] =>
    [...menu.querySelectorAll<HTMLButtonElement>('.sn-input-menu-item:not(:disabled)')]

  // 键盘入口：打开即聚焦第一项，方向键 / Home / End 在可用项间移动。
  enabledItems()[0]?.focus({ preventScroll: true })

  const onPointerDown = (event: PointerEvent): void => {
    if (event.target instanceof Node && menu.contains(event.target)) return
    closeInputContextMenu()
  }
  const onKeyDown = (event: KeyboardEvent): void => {
    if (event.key === 'Escape') {
      event.stopPropagation()
      // 关菜单把焦点交还控件，键盘用户不用再找回来。
      editable.focus({ preventScroll: true })
      closeInputContextMenu()
      return
    }
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp'
      && event.key !== 'Home' && event.key !== 'End') return
    const buttons = enabledItems()
    if (buttons.length === 0) return
    event.preventDefault()
    const current = buttons.indexOf(document.activeElement as HTMLButtonElement)
    const next = event.key === 'Home'
      ? 0
      : event.key === 'End'
        ? buttons.length - 1
        : event.key === 'ArrowDown'
          ? (current < 0 ? 0 : (current + 1) % buttons.length)
          : (current < 0 ? buttons.length - 1 : (current - 1 + buttons.length) % buttons.length)
    buttons[next]?.focus({ preventScroll: true })
  }
  const onClose = (): void => { closeInputContextMenu() }
  document.addEventListener('pointerdown', onPointerDown, true)
  document.addEventListener('keydown', onKeyDown, true)
  window.addEventListener('scroll', onClose, true)
  window.addEventListener('resize', onClose)
  window.addEventListener('blur', onClose)
  disposeOpen = () => {
    document.removeEventListener('pointerdown', onPointerDown, true)
    document.removeEventListener('keydown', onKeyDown, true)
    window.removeEventListener('scroll', onClose, true)
    window.removeEventListener('resize', onClose)
    window.removeEventListener('blur', onClose)
  }
}

/**
 * 安装输入框右键菜单。
 * @returns 卸载函数（移除监听并关闭菜单）。
 */
export function installInputContextMenu(): () => void {
  const onContextMenu = (event: MouseEvent): void => {
    if (openTarget !== null && openMenu?.contains(event.target as Node) === true) return
    const editable = editableTarget(event.target)
    if (editable === undefined) return
    event.preventDefault()
    showMenu(editable, event.clientX, event.clientY)
  }
  document.addEventListener('contextmenu', onContextMenu)
  return () => {
    document.removeEventListener('contextmenu', onContextMenu)
    closeInputContextMenu()
  }
}
