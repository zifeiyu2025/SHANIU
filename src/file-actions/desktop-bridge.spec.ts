import { describe, expect, it, vi } from 'vitest'
import { bytesToBase64, installDesktopFileActions, type DesktopFileActionsBridge } from './desktop-bridge'

const fakeFetch = (bytes: number[], type = 'image/png') => vi.fn(async () => ({
  ok: true,
  status: 200,
  arrayBuffer: async () => new Uint8Array(bytes).buffer,
  headers: { get: () => type },
})) as unknown as typeof fetch

describe('桌面文件动作适配面（SH-092 批 2）', () => {
  it('base64 分块编码与上游 Saved 形状', async () => {
    expect(bytesToBase64(new Uint8Array([104, 101, 108, 108, 111]))).toBe('aGVsbG8=')
    const invoke = vi.fn(async () => 'C:/saved/a.txt')
    vi.stubGlobal('fetch', fakeFetch([1, 2, 3]))
    const dispose = installDesktopFileActions({ invoke: invoke as unknown as DesktopFileActionsBridge['invoke'] })
    const actions = (globalThis as { dshDesktop?: { fileActions?: { save: (r: unknown) => Promise<unknown>; readClipboardImage: (u: string) => Promise<unknown> } } }).dshDesktop!.fileActions!
    await expect(actions.save({ url: 'blob:x', name: 'a.txt', mediaType: 'text/plain', mode: 'saveAs' }))
      .resolves.toEqual({ status: 'saved', path: 'C:/saved/a.txt' })
    expect(invoke).toHaveBeenCalledWith('save_file_bytes', { defaultName: 'a.txt', bytes: 'AQID' })
    dispose()
    vi.unstubAllGlobals()
  })

  it('取消对话框不伪造保存结果，卸载后适配面消失', async () => {
    const invoke = vi.fn(async () => null)
    vi.stubGlobal('fetch', fakeFetch([9]))
    const dispose = installDesktopFileActions({ invoke: invoke as unknown as DesktopFileActionsBridge['invoke'] })
    const host = globalThis as { dshDesktop?: { fileActions?: { save: (r: unknown) => Promise<unknown> } } }
    await expect(host.dshDesktop!.fileActions!.save({ url: 'blob:x', name: 'a.bin', mode: 'download' }))
      .resolves.toEqual({ status: 'cancelled' })
    dispose()
    expect(host.dshDesktop).toBeUndefined()
    vi.unstubAllGlobals()
  })

  it('复制图片回退读取原始字节与响应 MIME', async () => {
    vi.stubGlobal('fetch', fakeFetch([137, 80, 78, 71], 'image/png'))
    const dispose = installDesktopFileActions({ invoke: vi.fn() })
    const host = globalThis as { dshDesktop?: { fileActions?: { readClipboardImage: (u: string) => Promise<{ bytes: Uint8Array; mediaType: string }> } } }
    const content = await host.dshDesktop!.fileActions!.readClipboardImage('https://example.test/a.png')
    expect(Array.from(content.bytes)).toEqual([137, 80, 78, 71])
    expect(content.mediaType).toBe('image/png')
    dispose()
    vi.unstubAllGlobals()
  })
})