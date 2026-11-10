/**
 * 宿主自绘控件的词典（窗口控制层、输入框右键菜单）。
 *
 * 这些模块直接操作 DOM，运行在 DSH 的 locale 注入面之外（标题栏在 boot 前就挂载），
 * 拿不到 `t` 标准座位。因此这里按 `<html lang>` 现读：DSH locale 插件切换语言时会
 * 同步这个属性，新建的菜单自然跟着变；已挂载的静态 aria 由调用方监听 `lang` 重刷。
 *
 * 只维护中英两语（见 `docs/08` §5.5：扩语种明确排除，不是待办）。
 */
const ZH = {
  'chrome.minimize': '最小化',
  'chrome.maximize': '最大化',
  'chrome.restore': '还原',
  'chrome.close': '关闭',
  'contextMenu.label': '编辑',
  'contextMenu.undo': '撤销',
  'contextMenu.redo': '重做',
  'contextMenu.cut': '剪切',
  'contextMenu.copy': '复制',
  'contextMenu.paste': '粘贴',
  'contextMenu.selectAll': '全选',
} as const

const EN: Record<keyof typeof ZH, string> = {
  'chrome.minimize': 'Minimize',
  'chrome.maximize': 'Maximize',
  'chrome.restore': 'Restore',
  'chrome.close': 'Close',
  'contextMenu.label': 'Edit',
  'contextMenu.undo': 'Undo',
  'contextMenu.redo': 'Redo',
  'contextMenu.cut': 'Cut',
  'contextMenu.copy': 'Copy',
  'contextMenu.paste': 'Paste',
  'contextMenu.selectAll': 'Select all',
}

export type HostTextKey = keyof typeof ZH

/**
 * 取当前界面语言的一条宿主文案。
 *
 * `documentElement.lang` 为空时按中文（应用默认语言）处理，只有显式 `en*` 才走英文：
 * 这样 boot 早期（locale 尚未写入 lang）不会闪成英文。
 * @param key - 词典键。
 * @returns 对应语言的整句。
 */
export function hostT(key: HostTextKey): string {
  const lang = document.documentElement.lang.toLowerCase()
  return lang.startsWith('en') ? EN[key] : ZH[key]
}
