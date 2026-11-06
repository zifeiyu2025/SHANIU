/**
 * 剪贴板桥：桌面壳走 Tauri 命令，纯浏览器退回 Clipboard API。
 *
 * 桌面必须走宿主：WebView2 对 `navigator.clipboard.readText()` 走权限提示，
 * 无提示被拒时粘贴会静默失败；Rust 侧直接读系统剪贴板，行为与原生输入框一致。
 */

/** 是否运行在 Tauri 壳内（与 main.tsx / open-workspace 同一判据）。 */
function inTauri(): boolean {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

/**
 * 读取系统剪贴板的纯文本。
 * @returns 剪贴板文本；读不到时返回空串（调用方按「无可粘贴内容」处理）。
 */
export async function readClipboardText(): Promise<string> {
  if (inTauri()) {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke<string>('read_clipboard_text')
  }
  return await navigator.clipboard.readText()
}

/**
 * 写入系统剪贴板的纯文本（execCommand 复制失败时的兜底）。
 * @param text - 要写入的文本。
 */
export async function writeClipboardText(text: string): Promise<void> {
  if (inTauri()) {
    const { invoke } = await import('@tauri-apps/api/core')
    await invoke('write_clipboard_text', { text })
    return
  }
  await navigator.clipboard.writeText(text)
}
