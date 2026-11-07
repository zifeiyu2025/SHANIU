/**
 * 页面内的 `/open-in-app/*` 路由垫片（SH-092 批 3）。
 *
 * 上游 `ui-open-in-app` 的目录目录/启动/图标三个入口是 Host 的 HTTP 路由
 * （`dsh-host-open-in-app`）。桌面壳没有本地 HTTP 服务，但 vendored 客户端只经
 * 全局 `fetch` 访问这三条相对路径——在装配期把它们接到 Tauri 命令，保持上游源码
 * 逐字节不变。图标不做提取（宿主无图像解码），一律 404，vendored 组件按自己的
 * 回退规则画原生图钉。
 */
export interface OpenInAppRouteBridge {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>
}

const json = (body: unknown, status = 200): Response =>
  new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } })

export function installOpenInAppRoutes(bridge: OpenInAppRouteBridge): () => void {
  const original = globalThis.fetch
  const routed = async (
    input: Parameters<typeof fetch>[0],
    init?: Parameters<typeof fetch>[1],
  ): Promise<Response> => {
    const raw = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    const pathname = new URL(raw, globalThis.location.href).pathname
    if (pathname === '/open-in-app/apps') {
      try {
        return json({ apps: await bridge.invoke<string[]>('open_in_app_catalog') })
      } catch {
        return new Response(null, { status: 503 })
      }
    }
    if (pathname === '/open-in-app/open') {
      try {
        const payload = JSON.parse(String(init?.body ?? '{}')) as { app?: unknown; path?: unknown }
        if (typeof payload.app !== 'string' || typeof payload.path !== 'string') {
          return new Response(null, { status: 400 })
        }
        await bridge.invoke('open_in_app_launch', { app: payload.app, path: payload.path })
        return json({ opened: true })
      } catch {
        return new Response(null, { status: 502 })
      }
    }
    if (pathname.startsWith('/open-in-app/icon/')) {
      return new Response(null, { status: 404 })
    }
    return original(input, init)
  }
  globalThis.fetch = routed as typeof fetch
  return () => {
    globalThis.fetch = original
  }
}