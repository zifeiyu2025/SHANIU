/**
 * 桌面壳的 `dshDesktop.fileActions` 适配面（上游 ui-file-actions 的 DesktopFileActions 契约）。
 *
 * **保存路径只由原生对话框决定**：命令内部弹「另存为」，前端交不出落盘路径——把路径当参数
 * 传入等于给 Web 内容一个任意写能力。下游 `download` 与 `saveAs` 都走同一个对话框：
 * 桌面壳没有 Chromium 下载管理器，静默写 Downloads 会绕过用户选择（与上游桌面版的偏差）。
 *
 * `readClipboardImage` 只服务跨域 http(s) 原图的复制图片回退；本地资源（blob:/data:/dsh-app:）
 * 走同一 fetch，不经此面。
 */
export interface DesktopFileActionsBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface DesktopFileActions {
  save(request: {
    url: string
    name?: string
    mediaType?: string
    mode: 'download' | 'saveAs'
  }): Promise<{ status: 'cancelled' } | { status: 'saved'; path: string }>
  readClipboardImage(url: string): Promise<{ bytes: Uint8Array; mediaType: string }>
}

const CHUNK = 0x8000

/** 分块编码：32 MiB 的字节直接 `String.fromCharCode(...bytes)` 会撑爆调用栈。 */
export function bytesToBase64(bytes: Uint8Array): string {
  let binary = ''
  for (let offset = 0; offset < bytes.length; offset += CHUNK) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + CHUNK))
  }
  return btoa(binary)
}

export function installDesktopFileActions(bridge: DesktopFileActionsBridge): () => void {
  const fileActions: DesktopFileActions = {
    async save(request) {
      const response = await fetch(request.url)
      if (!response.ok) throw new Error(`原始文件不可读（HTTP ${response.status}）`)
      const bytes = new Uint8Array(await response.arrayBuffer())
      const path = await bridge.invoke<string | null>('save_file_bytes', {
        defaultName: request.name ?? 'download',
        bytes: bytesToBase64(bytes),
      })
      return path === null ? { status: 'cancelled' } : { status: 'saved', path }
    },
    async readClipboardImage(url) {
      const response = await fetch(url, { credentials: 'omit' })
      if (!response.ok) throw new Error(`原始图片不可读（HTTP ${response.status}）`)
      return {
        bytes: new Uint8Array(await response.arrayBuffer()),
        mediaType: response.headers.get('content-type') ?? '',
      }
    },
  }
  const host = globalThis as { dshDesktop?: { fileActions?: unknown } }
  const previous = host.dshDesktop
  host.dshDesktop = { ...(previous ?? {}), fileActions }
  return () => {
    if (previous === undefined) delete host.dshDesktop
    else host.dshDesktop = previous
  }
}