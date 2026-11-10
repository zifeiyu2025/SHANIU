/**
 * `window.open` 的宿主垫片（SH-137）。
 *
 * WebView 里没有「新标签页」：原生 `window.open('https://…')` 会把**整个应用**
 * 导航到外网（返回按钮都没有），等于把壳变成浏览器。上游三处外链入口
 * （`ui-chat/apply.ts:285`、`ui-open-in-app/context-menus.tsx:121`、
 * `ui-sidebar-browser/BrowserBody.tsx:101`）都只经全局 `window.open`，
 * 所以在装配期把全局换成 Tauri 命令，vendored 源码保持逐字节不变。
 *
 * 只放行绝对 http/https：`javascript:` / `data:` / `file:` 一律吞掉——它们要么
 * 让页面拿到执行能力，要么越出工作区边界。Rust 侧 `open_external_url` 再判一次。
 */
export interface ExternalLinkBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>
}

/** 绝对 http(s) URL 解析结果；其余（相对路径、非 http 协议、非法串）一律 `undefined`。 */
export function externalHttpUrl(raw: string): string | undefined {
  let parsed: URL
  try {
    parsed = new URL(raw)
  } catch {
    return undefined
  }
  return parsed.protocol === 'http:' || parsed.protocol === 'https:' ? parsed.href : undefined
}

/**
 * 把 `window.open` 换成宿主出口。
 * @param bridge - 宿主命令桥（桌面壳为 Tauri invoke）。
 * @returns 还原器；装配拆卸时调用。
 */
export function installExternalLinkOpener(bridge: ExternalLinkBridge): () => void {
  const original = globalThis.open
  const routed = (url?: string | URL, _target?: string, _features?: string): null => {
    const raw = typeof url === 'string' ? url : url?.href
    const href = raw === undefined ? undefined : externalHttpUrl(raw)
    // 返回值恒为 null：即使被拒，调用方也拿不到可导航的 Window 引用。
    if (href === undefined) return null
    void bridge.invoke('open_external_url', { url: href }).catch(() => undefined)
    return null
  }
  globalThis.open = routed as typeof globalThis.open
  return () => {
    globalThis.open = original
  }
}
