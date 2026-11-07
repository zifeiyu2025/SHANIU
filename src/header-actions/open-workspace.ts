/** 系统文件管理器桥接：只有 Tauri 宿主能真的打开目录。 */

/** 与 main.tsx 装窗口控制层同一判定：页面里有没有 Tauri 的内部对象。 */
export function inTauri(): boolean {
  return '__TAURI_INTERNALS__' in window
}

/**
 * 在系统文件管理器里打开一个目录。
 *
 * 浏览器 dev 环境（5173 直接开页面）没有这个能力，返回 false 让调用方保持静默，
 * 不去改跳右侧栏——那是另一件事。
 * @param path - 要打开的目录绝对路径。
 * @returns 是否真的发起了打开动作。
 */
export async function openInSystemExplorer(path: string): Promise<boolean> {
  if (!inTauri()) return false
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('open_in_explorer', { path })
  return true
}
