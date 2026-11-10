/**
 * 桌面壳的附件上传载体（SH-116 批 1）。
 *
 * `file-upload` 客户端默认在 Worker 里向相对路由 `/api/session/uploadFileBinary` 发
 * XHR；桌面壳没有 HTTP 服务，所以它支持一个启动前的 `__DSH_FILE_UPLOAD__` fetch 钩子。
 * 这里把该路由接到 Tauri 命令 `upload_attachment`（宿主只暂存本地字节，不写 rollout）。
 *
 * 批 1 接文件与图片的**暂存**：文件按原字节保存；图片先用浏览器解码尺寸再交给宿主
 * 保存原字节（不做归一化）。prompt 侧把图片只作为占位，历史图片回显留给批 2。
 */
const UPLOAD_ROUTE = '/api/session/uploadFileBinary'

interface StoredAttachment {
  readonly receiptId: string
  readonly file: { readonly attachmentId: string; readonly name: string; readonly bytes: number }
}

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

function jsonResponse(value: unknown): Response {
  return new Response(JSON.stringify(value), {
    status: 200,
    headers: { 'content-type': 'application/json' },
  })
}

function failure(code: string, message: string): Response {
  return jsonResponse({ ok: false, error: { code, message, details: {} } })
}

/** 请求体可能来自 Blob、精确字节或一次性流——一并读成 Uint8Array。 */
async function readBody(body: BodyInit | null | undefined): Promise<Uint8Array> {
  if (body === null || body === undefined) return new Uint8Array()
  if (typeof Blob !== 'undefined' && body instanceof Blob) {
    return new Uint8Array(await body.arrayBuffer())
  }
  if (body instanceof ArrayBuffer) return new Uint8Array(body)
  if (ArrayBuffer.isView(body)) {
    return new Uint8Array(body.buffer, body.byteOffset, body.byteLength)
  }
  if (typeof ReadableStream !== 'undefined' && body instanceof ReadableStream) {
    const reader = (body as ReadableStream<Uint8Array>).getReader()
    const chunks: Uint8Array[] = []
    let total = 0
    for (;;) {
      const { done, value } = await reader.read()
      if (done) break
      chunks.push(value)
      total += value.byteLength
    }
    const out = new Uint8Array(total)
    let offset = 0
    for (const chunk of chunks) {
      out.set(chunk, offset)
      offset += chunk.byteLength
    }
    return out
  }
  if (typeof body === 'string') return new TextEncoder().encode(body)
  throw new Error('不支持的附件请求体')
}

function bytesToBase64(bytes: Uint8Array): string {
  let binary = ''
  const chunk = 0x8000
  for (let index = 0; index < bytes.length; index += chunk) {
    binary += String.fromCharCode(...bytes.subarray(index, index + chunk))
  }
  return btoa(binary)
}

/**
 * 处理一次 file-upload 的 fetch 调用。导出为纯函数便于单测；`install` 只负责注入。
 * @param invoke - Tauri invoke（测试注入假实现）。
 * @param input - 文档相对上传路由（含 query）。
 * @param init - 原始请求体与取消信号。
 * @returns 宿主回执或显式业务错误（HTTP 200，形状同上游 route）。
 */
export async function handleAttachmentUpload(
  invoke: Invoke,
  input: string | URL,
  init: RequestInit,
): Promise<Response> {
  const url = new URL(String(input), 'http://localhost')
  if (!url.pathname.endsWith(UPLOAD_ROUTE)) return new Response(null, { status: 404 })
  const sessionId = url.searchParams.get('sessionId') ?? ''
  if (sessionId === '') return failure('attachments/bad-request', '附件上传缺少 sessionId')
  const name = url.searchParams.get('name') ?? 'attachment'
  const imageMediaType = url.searchParams.get('mediaType')
  if (init.signal?.aborted === true) {
    return failure('attachments/cancelled', '附件上传已取消')
  }
  try {
    const bytes = await readBody(init.body)
    if (bytes.byteLength === 0) return failure('attachments/empty', '附件为空')
    if (imageMediaType !== null && imageMediaType !== '') {
      // 图片尺寸由浏览器解码，宿主只保存原字节；批 2 再补归一化/历史回显。
      if (typeof createImageBitmap !== 'function') {
        return failure('attachments/image-decode-failed', '当前 WebView 不支持图片解码')
      }
      let width = 0
      let height = 0
      try {
        const copy = new Uint8Array(bytes.byteLength)
        copy.set(bytes)
        const bitmap = await createImageBitmap(new Blob([copy], { type: imageMediaType }))
        width = bitmap.width
        height = bitmap.height
        bitmap.close()
      } catch (reason) {
        const message = reason instanceof Error ? reason.message : String(reason)
        return failure('attachments/image-decode-failed', `图片解码失败：${message}`)
      }
      if (width <= 0 || height <= 0) return failure('attachments/image-decode-failed', '图片尺寸无效')
      const stored = await invoke('upload_attachment', {
        sessionId,
        name,
        mediaType: imageMediaType,
        data: bytesToBase64(bytes),
        width,
        height,
      }) as StoredAttachment
      return jsonResponse({ ok: true, value: stored })
    }
    const stored = await invoke('upload_attachment', {
      sessionId,
      name,
      mediaType: 'application/octet-stream',
      data: bytesToBase64(bytes),
    }) as StoredAttachment
    return jsonResponse({ ok: true, value: stored })
  } catch (reason) {
    const message = reason instanceof Error ? reason.message : String(reason)
    return failure('attachments/upload-failed', message)
  }
}

/**
 * 在 file-upload 插件构造前注入 fetch 钩子（桌面真实数据源路径调用）。
 * @returns 还原器：撤回钩子并恢复安装前的全局值（boot 的 dispose 必须调用它）。
 */
export function installAttachmentUploadFetch(): () => void {
  const invoke: Invoke = async (command, args) => {
    const { invoke: tauriInvoke } = await import('@tauri-apps/api/core')
    return await tauriInvoke(command, args)
  }
  const host = globalThis as { __DSH_FILE_UPLOAD__?: unknown }
  const previous = host.__DSH_FILE_UPLOAD__
  host.__DSH_FILE_UPLOAD__ = {
    fetch: (input: string | URL, init: RequestInit) => handleAttachmentUpload(invoke, input, init),
  }
  return () => {
    if (previous === undefined) delete host.__DSH_FILE_UPLOAD__
    else host.__DSH_FILE_UPLOAD__ = previous
  }
}
