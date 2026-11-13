/**
 * 横幅点击后的定位动作：把审批卡滚进视野并聚焦。
 *
 * 为什么不用 `request_id` 直接选：vendored `ApprovalCard` 上的 `data-approval-key`
 * 是它自己生成的展示键（`approval:N`），不是后端签发的 `request_id`，哪个键对应
 * 哪条请求只有卡片自己知道。而 composer 一次只展示一张待审批卡，所以「先切到
 * 那条请求所属的会话，再找会话里唯一的那张卡」就够了——不需要改 vendored 代码，
 * 也不会因为多会话共用窗体而点错（会话由横幅每行自带的 `sessionId` 决定，
 * 与"当前会话"无关）。
 *
 * 切换会话是异步的：旧会话的卡可能还在 DOM 里。因此点击时先记下旧卡，重试期间
 * 只接受**换了对象**的卡；若目标会话本来就展示着同一张卡（`openSession` 对当前
 * 会话是空操作），给一个短宽限期后直接接受它。
 */

/** 审批卡的 DOM 锚点（vendored `ApprovalCard` 已有）。 */
export const APPROVAL_CARD_SELECTOR = '[data-approval-key]'

/** 重试间隔：会话切换通常在几帧内完成。 */
const FOCUS_RETRY_MS = 50
/** 总宽限：超过这个时间还没等到卡，就放弃（横幅仍在，用户可再点）。 */
const FOCUS_TIMEOUT_MS = 2_000
/** 高亮持续时间：只是提示"是这张"，不改变卡片状态。 */
const FOCUS_HIGHLIGHT_MS = 2_400
/** 同一张卡（目标会话已是当前会话）的等待宽限。 */
const SAME_CARD_GRACE_MS = 500

/** 高亮态类名：样式见 `approval-banner.css`。 */
export const FOCUS_CLASS = 'shaniu-approval-focus'

/**
 * 聚焦到当前可见的审批卡。
 *
 * @param doc - 文档对象（测试注入 jsdom 文档外的替身）。
 * @param staleCard - 点击前就已在 DOM 里的卡；切换会话后它应当被换掉。
 * @returns 撤销入口：调用方（卸载/再次点击）可以停止旧的重试，避免两个定时器抢焦点。
 */
export function focusApprovalCard(
  doc: Document = document,
  staleCard: Element | null = null,
): () => void {
  const startedAt = Date.now()
  let timer: ReturnType<typeof setTimeout> | undefined

  const tick = (): void => {
    const card = doc.querySelector<HTMLElement>(APPROVAL_CARD_SELECTOR)
    const settled = card !== null
      && (card !== staleCard || Date.now() - startedAt > SAME_CARD_GRACE_MS)
    if (!settled) {
      if (Date.now() - startedAt < FOCUS_TIMEOUT_MS) {
        timer = setTimeout(tick, FOCUS_RETRY_MS)
      }
      return
    }
    card.scrollIntoView?.({ block: 'center', behavior: 'smooth' })
    card.focus?.({ preventScroll: true })
    card.classList.add(FOCUS_CLASS)
    setTimeout(() => { card.classList.remove(FOCUS_CLASS) }, FOCUS_HIGHLIGHT_MS)
  }

  timer = setTimeout(tick, 0)
  // 返回撤销入口：调用方（卸载/再次点击）可以停止旧的重试，避免两个定时器抢焦点。
  return () => {
    if (timer !== undefined) clearTimeout(timer)
  }
}

/** 点击前的卡：用于区分"旧会话的卡"与"切换后新会话的卡"。 */
export function currentApprovalCard(doc: Document = document): Element | null {
  return doc.querySelector(APPROVAL_CARD_SELECTOR)
}
