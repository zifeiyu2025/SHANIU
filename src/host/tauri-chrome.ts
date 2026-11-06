/**
 * WindowControls 的 Tauri 实现：把控制层的注入面接到 @tauri-apps/api 的真实窗口。
 * 单独一层是因为 chrome.ts 要在 jsdom 下拿假 controls 断言调用，而这里的价值只有
 * 「映射对不对」，没有可测的行为。
 */
import { getCurrentWindow } from '@tauri-apps/api/window'
import { mountWindowChrome, type WindowControls } from './chrome'
import './chrome.css'

/** 在当前窗口装上标题按钮、拖拽与缩放手柄，返回卸载函数。 */
export function installWindowChrome(): () => void {
  const win = getCurrentWindow()
  const controls: WindowControls = {
    minimize: () => win.minimize(),
    toggleMaximize: () => win.toggleMaximize(),
    close: () => win.close(),
    startDragging: () => win.startDragging(),
    startResizeDragging: edge => win.startResizeDragging(edge),
    isMaximized: () => win.isMaximized(),
  }
  return mountWindowChrome(controls)
}
