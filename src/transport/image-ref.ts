/**
 * 档案图片引用 → 展示引用的唯一转换点。
 *
 * 为什么单独成文件：`adapter.ts`（实时 journal）与 `header-actions/session-events.ts`
 * （导出/复制 Markdown）都在解释同一份 `load_session` 档案里的 `images`。两处各写一份
 * 转换，字段或过滤规则一改就会漂移，且漂移只在「导出」这种低频路径上暴露。
 */

/**
 * 档案里图片引用的 wire 形状（`src-tauri/src/images/mod.rs` 的 `ImageRef`）。
 * 字段名跟 Rust serde 走，不重命名；`attachment_id` / `bytes` 是后加的可选键
 * （旧档案没有）。
 */
export interface SessionImageRefWire {
  readonly path: string
  readonly sha256: string
  readonly width: number
  readonly height: number
  readonly tokens: number
  readonly mime: string
  readonly attachment_id?: string
  readonly bytes?: number
}

/** 持久图片引用：与 DSH `ImageAttachmentRef` 同形，随 user/message 内容落 journal。 */
export interface PromptImageRef {
  readonly attachmentId: string
  readonly mediaType: string
  readonly bytes: number
  readonly width: number
  readonly height: number
  readonly name?: string
}

/**
 * 档案引用 → 展示引用（与 DSH `ImageAttachmentRef` 同形）。落档时没有保存文件名，
 * 这里省略 `name`；没有 `attachment_id` 的引用（`view_image` 回填）没有可回读的
 * 附件来源，不能当成可解析的图片块，直接丢弃。
 */
export function archiveImageToPrompt(ref: SessionImageRefWire): PromptImageRef | undefined {
  if (ref.attachment_id === undefined) return undefined
  return {
    attachmentId: ref.attachment_id,
    mediaType: ref.mime,
    bytes: ref.bytes ?? 0,
    width: ref.width,
    height: ref.height,
  }
}

/**
 * 图片引用 → `user/message` 的 `image` 内容块。
 *
 * 三个地方都要铺同样的块：实时提交、回合收口的排队补发、旧档案回放。块键写错
 * （例如把 `attachment` 写成 `ref`）不会报错，只会让模型收到一个空块、界面却显示
 * 已发送——所以形状只在这里构造一次。
 */
export function imageContentBlocks(
  images: readonly PromptImageRef[],
): { type: 'image'; attachment: PromptImageRef }[] {
  return images.map(image => ({ type: 'image' as const, attachment: image }))
}
