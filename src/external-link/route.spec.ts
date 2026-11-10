import { describe, expect, it, vi } from 'vitest'
import { externalHttpUrl, installExternalLinkOpener } from './route'

const bridgeOf = (invoke: (...args: unknown[]) => Promise<unknown>) =>
  ({ invoke: invoke as unknown as Parameters<typeof installExternalLinkOpener>[0]['invoke'] })

describe('外链宿主垫片（SH-137）', () => {
  it('http/https 交给宿主命令，返回值恒为 null（不导航应用）', () => {
    const invoke = vi.fn(async () => undefined)
    const native = vi.fn()
    vi.stubGlobal('open', native)
    const dispose = installExternalLinkOpener(bridgeOf(invoke))
    expect(globalThis.open('https://example.com/a?b=1', '_blank')).toBeNull()
    expect(invoke).toHaveBeenCalledWith('open_external_url', { url: 'https://example.com/a?b=1' })
    expect(native).not.toHaveBeenCalled()
    dispose()
    expect(globalThis.open).toBe(native)
    vi.unstubAllGlobals()
  })

  it('非 http(s)、相对路径与非法串一律吞掉，不调宿主也不导航', () => {
    const invoke = vi.fn(async () => undefined)
    const native = vi.fn()
    vi.stubGlobal('open', native)
    const dispose = installExternalLinkOpener(bridgeOf(invoke))
    for (const blocked of ['javascript:alert(1)', 'file:///C:/x.png', 'data:text/html,x', '/relative', 'example.com']) {
      expect(globalThis.open(blocked)).toBeNull()
    }
    expect(globalThis.open()).toBeNull()
    expect(invoke).not.toHaveBeenCalled()
    expect(native).not.toHaveBeenCalled()
    dispose()
    vi.unstubAllGlobals()
  })

  it('宿主命令失败不影响渲染，也不回退到原生 open', async () => {
    const invoke = vi.fn(async () => { throw new Error('host said no') })
    const native = vi.fn()
    vi.stubGlobal('open', native)
    const dispose = installExternalLinkOpener(bridgeOf(invoke))
    expect(globalThis.open('http://example.com/x')).toBeNull()
    await Promise.resolve()
    expect(native).not.toHaveBeenCalled()
    dispose()
    vi.unstubAllGlobals()
  })

  it('externalHttpUrl 只认绝对 http(s)', () => {
    expect(externalHttpUrl('HTTPS://EXAMPLE.COM/a')).toBe('https://example.com/a')
    expect(externalHttpUrl('http://example.com')).toBe('http://example.com/')
    for (const denied of ['mailto:a@b.c', 'file:///x', '//example.com/a', '/a', 'https://']) {
      expect(externalHttpUrl(denied)).toBeUndefined()
    }
  })
})
