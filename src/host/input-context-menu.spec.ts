import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import { closeInputContextMenu, installInputContextMenu } from './input-context-menu'

let dispose: (() => void) | undefined

/** 发一次右键；返回原始事件用于断言默认行为是否被接管。 */
function fireContextMenu(target: Element): MouseEvent {
  const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 20, clientY: 30 })
  target.dispatchEvent(event)
  return event
}

const menu = (): HTMLElement | null => document.querySelector('.sn-input-menu')
const items = (): HTMLButtonElement[] => [...document.querySelectorAll<HTMLButtonElement>('.sn-input-menu-item')]
const itemByLabel = (label: string): HTMLButtonElement | undefined =>
  items().find(button => button.firstElementChild?.textContent === label)

function textInput(value = ''): HTMLInputElement {
  const input = document.createElement('input')
  input.type = 'text'
  input.value = value
  document.body.append(input)
  return input
}

beforeEach(() => {
  document.body.innerHTML = ''
  dispose = installInputContextMenu()
})

afterEach(() => {
  dispose?.()
  dispose = undefined
  closeInputContextMenu()
})

describe('输入框右键菜单', () => {
  it('在文本输入框上右键接管默认菜单并列出六项编辑命令', () => {
    const input = textInput('hello')
    const event = fireContextMenu(input)

    expect(event.defaultPrevented).toBe(true)
    expect(menu()).not.toBeNull()
    expect(items().map(button => button.firstElementChild?.textContent)).toEqual([
      '撤销', '重做', '剪切', '复制', '粘贴', '全选',
    ])
  })

  it('非可编辑目标不接管', () => {
    const div = document.createElement('div')
    document.body.append(div)
    const event = fireContextMenu(div)
    expect(event.defaultPrevented).toBe(false)
    expect(menu()).toBeNull()
  })

  it('勾选类 input 不接管（保留原生交互）', () => {
    const checkbox = document.createElement('input')
    checkbox.type = 'checkbox'
    document.body.append(checkbox)
    const event = fireContextMenu(checkbox)
    expect(event.defaultPrevented).toBe(false)
    expect(menu()).toBeNull()
  })

  it('剪切/复制按选区启用；全选按内容启用', () => {
    const empty = textInput('')
    fireContextMenu(empty)
    expect(itemByLabel('剪切')?.disabled).toBe(true)
    expect(itemByLabel('复制')?.disabled).toBe(true)
    expect(itemByLabel('全选')?.disabled).toBe(true)

    closeInputContextMenu()
    const filled = textInput('hello')
    filled.setSelectionRange(1, 3)
    fireContextMenu(filled)
    expect(itemByLabel('剪切')?.disabled).toBe(false)
    expect(itemByLabel('复制')?.disabled).toBe(false)
    expect(itemByLabel('全选')?.disabled).toBe(false)
  })

  it('点「全选」选中控件全部内容并关闭菜单', () => {
    const input = textInput('hello')
    fireContextMenu(input)
    itemByLabel('全选')?.click()

    expect(menu()).toBeNull()
    expect(input.selectionStart).toBe(0)
    expect(input.selectionEnd).toBe(5)
  })

  it('Escape 与外部按下都会关闭菜单', () => {
    fireContextMenu(textInput('hello'))
    document.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
    expect(menu()).toBeNull()

    fireContextMenu(textInput('hello'))
    document.body.dispatchEvent(new MouseEvent('pointerdown', { bubbles: true }))
    expect(menu()).toBeNull()
  })

  it('键盘导航：方向键 / Home / End 在可用项间移动，Escape 交还焦点', () => {
    const input = textInput('hello')
    input.focus()
    fireContextMenu(input)
    const enabled = items().filter(button => !button.disabled)
    expect(enabled.length).toBeGreaterThan(1)
    expect(document.activeElement).toBe(enabled[0])

    const key = (value: string): void => {
      document.dispatchEvent(new KeyboardEvent('keydown', { key: value, bubbles: true }))
    }
    key('ArrowDown')
    expect(document.activeElement).toBe(enabled[1])
    key('End')
    expect(document.activeElement).toBe(enabled[enabled.length - 1])
    key('Home')
    expect(document.activeElement).toBe(enabled[0])
    key('ArrowUp')
    expect(document.activeElement).toBe(enabled[enabled.length - 1])

    key('Escape')
    expect(menu()).toBeNull()
    expect(document.activeElement).toBe(input)
  })

  it('重复右键只保留一个菜单', () => {
    fireContextMenu(textInput('a'))
    fireContextMenu(textInput('b'))
    expect(document.querySelectorAll('.sn-input-menu')).toHaveLength(1)
  })

  it('卸载后不再接管', () => {
    dispose?.()
    dispose = undefined
    const event = fireContextMenu(textInput('hello'))
    expect(event.defaultPrevented).toBe(false)
    expect(menu()).toBeNull()
  })
})
