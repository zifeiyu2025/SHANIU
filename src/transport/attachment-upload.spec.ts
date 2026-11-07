import { describe, expect, it, vi } from 'vitest'

import { handleAttachmentUpload } from './attachment-upload'

const stored = { receiptId: 'r1', file: { attachmentId: 'r1', name: 'a.txt', bytes: 2 } }

describe('attachment-upload（SH-116 批 1）', () => {
  it('把 Blob 字节经 upload_attachment 交给宿主，回执原样回给 file-upload', async () => {
    const invoke = vi.fn(async () => stored)
    const response = await handleAttachmentUpload(
      invoke,
      '/api/session/uploadFileBinary?sessionId=s1&name=a.txt',
      { body: new Blob(['hi']) },
    )
    expect(response.status).toBe(200)
    expect(await response.json()).toEqual({ ok: true, value: stored })
    expect(invoke).toHaveBeenCalledWith('upload_attachment', {
      sessionId: 's1',
      name: 'a.txt',
      mediaType: 'application/octet-stream',
      data: btoa('hi'),
    })
  })

  it('图片先解码尺寸再交给宿主，回执走 image 形状', async () => {
    const close = vi.fn()
    vi.stubGlobal('createImageBitmap', vi.fn(async () => ({ width: 2, height: 3, close })))
    const image = {
      receiptId: 'r2',
      image: { attachmentId: 'r2', name: 'a.png', mediaType: 'image/png', bytes: 1, width: 2, height: 3 },
    }
    const invoke = vi.fn(async () => image)
    const response = await handleAttachmentUpload(
      invoke,
      '/api/session/uploadFileBinary?sessionId=s1&name=a.png&mediaType=image%2Fpng',
      { body: new Blob(['x']) },
    )
    expect(await response.json()).toEqual({ ok: true, value: image })
    expect(invoke).toHaveBeenCalledWith('upload_attachment', {
      sessionId: 's1',
      name: 'a.png',
      mediaType: 'image/png',
      data: btoa('x'),
      width: 2,
      height: 3,
    })
    expect(close).toHaveBeenCalled()
    vi.unstubAllGlobals()
  })

  it('宿主失败与空附件都回业务错误（HTTP 200 形状同上游 route）', async () => {
    const failure = vi.fn(async () => {
      throw new Error('磁盘已满')
    })
    const failed = await handleAttachmentUpload(
      failure,
      '/api/session/uploadFileBinary?sessionId=s1&name=a.txt',
      { body: new Blob(['x']) },
    )
    expect(await failed.json()).toMatchObject({ ok: false, error: { code: 'attachments/upload-failed', message: '磁盘已满' } })

    const invoke = vi.fn(async () => stored)
    const empty = await handleAttachmentUpload(
      invoke,
      '/api/session/uploadFileBinary?sessionId=s1&name=a.txt',
      { body: new Blob([]) },
    )
    expect(await empty.json()).toMatchObject({ ok: false, error: { code: 'attachments/empty' } })
    expect(invoke).not.toHaveBeenCalled()
  })

  it('非上传路由回 404，缺 sessionId 回 400 业务错误', async () => {
    const invoke = vi.fn(async () => stored)
    const missing = await handleAttachmentUpload(invoke, '/api/other', { body: null })
    expect(missing.status).toBe(404)
    const noSession = await handleAttachmentUpload(
      invoke,
      '/api/session/uploadFileBinary?name=a.txt',
      { body: new Blob(['x']) },
    )
    expect(await noSession.json()).toMatchObject({ ok: false, error: { code: 'attachments/bad-request' } })
  })
})
