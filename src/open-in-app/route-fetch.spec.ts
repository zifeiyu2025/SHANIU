import { describe, expect, it, vi } from 'vitest'
import { installOpenInAppRoutes } from './route-fetch'

describe('/open-in-app 路由垫片（SH-092 批 3）', () => {
  it('apps 路由把宿主目录包成上游 payload', async () => {
    const invoke = vi.fn(async () => ['explorer', 'vscode'])
    const dispose = installOpenInAppRoutes({ invoke: invoke as unknown as Parameters<typeof installOpenInAppRoutes>[0]['invoke'] })
    const response = await fetch('/open-in-app/apps')
    expect(response.status).toBe(200)
    await expect(response.json()).resolves.toEqual({ apps: ['explorer', 'vscode'] })
    expect(invoke).toHaveBeenCalledWith('open_in_app_catalog')
    dispose()
  })

  it('open 路由校验载荷并转发 app/path', async () => {
    const invoke = vi.fn(async () => undefined)
    const dispose = installOpenInAppRoutes({ invoke: invoke as unknown as Parameters<typeof installOpenInAppRoutes>[0]['invoke'] })
    const response = await fetch('/open-in-app/open', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ app: 'vscode', path: 'D:/ws' }),
    })
    expect(response.status).toBe(200)
    expect(invoke).toHaveBeenCalledWith('open_in_app_launch', { app: 'vscode', path: 'D:/ws' })
    const bad = await fetch('/open-in-app/open', { method: 'POST', body: '{"app":1}' })
    expect(bad.status).toBe(400)
    dispose()
  })

  it('宿主拒绝时回失败状态，图标一律 404，其余请求透传', async () => {
    const original = vi.fn(async () => new Response('passthrough', { status: 200 }))
    vi.stubGlobal('fetch', original)
    const invoke = vi.fn(async () => { throw new Error('host said no') })
    const dispose = installOpenInAppRoutes({ invoke: invoke as unknown as Parameters<typeof installOpenInAppRoutes>[0]['invoke'] })
    expect((await fetch('/open-in-app/open', { method: 'POST', body: '{"app":"vscode","path":"D:/ws"}' })).status).toBe(502)
    expect((await fetch('/open-in-app/apps')).status).toBe(503)
    expect((await fetch('/open-in-app/icon/vscode')).status).toBe(404)
    const passthrough = await fetch('/elsewhere.json')
    expect(await passthrough.text()).toBe('passthrough')
    expect(original).toHaveBeenCalledTimes(1)
    dispose()
    vi.unstubAllGlobals()
  })
})