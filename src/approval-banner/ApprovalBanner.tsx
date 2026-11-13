/**
 * 全局待审批横幅：审批卡是否"看得见"的最后一道保险。
 *
 * 为什么必须存在：审批卡只出现在**它所属会话**的 composer 上。用户正在别的会话
 * 里、或窗口被其他会话占满时，后端其实已经挂起等回执，但界面上没有任何线索——
 * 看起来就像"卡住了"。横幅把「有几条、在哪个会话、是什么工具」摊到全局可见处，
 * 并且点一行就切到那个会话、把卡滚进视野。
 *
 * 多会话共用同一个窗体时的定位口径：点击只用该行自带的 `sessionId`，
 * **不读"当前会话"、不按下标猜**。同一条会话里有多条请求时，composer 一次只
 * 展示一张，切换后看到的就是那一张；横幅行本身带工具名，用户能确认是哪条。
 *
 * 心跳中断（`attended = false`）时额外说清后果：TTL 到点由宿主统一拒绝，
 * 不是"再等等就好"。这条文案只在真的断心跳时出现，常态不占位。
 */
import { useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import { NS } from './locales'
import { hostApprovalBannerWire, type ApprovalBannerWire } from './wire'
import type { PendingApprovalsSnapshotWire } from '../transport/pending-approval'
import './approval-banner.css'

/** 桌面运行时的默认数据面（模块级常量：保持引用稳定，避免 effect 每次重跑）。 */
const DEFAULT_WIRE = hostApprovalBannerWire()

/** 最多展示几行：再多就不是"路过看见"而是第二个审批列表了。 */
const MAX_ROWS = 3

/** 横幅拿到的 props：根部标准套件 + 本命名空间词典 + 可注入的数据面与导航。 */
export type ApprovalBannerProps = PropsRuntime<'shell.overlay'> & PropsLocale<typeof NS> & {
  /** 待审批数据面；测试注入脚本化实现。 */
  readonly wire?: ApprovalBannerWire
  /** 会话标题解析（`ctx.sessions`）；解析不到时退回会话 id。 */
  readonly titleOf?: (sessionId: string) => string
  /** 切换到目标会话（`ctx.uiWorkspace.openSession`）。 */
  readonly openSession?: (sessionId: string) => void
  /** 聚焦到审批卡；缺省用 DOM 锚点扫描（见 `focus.ts`）。 */
  readonly focusApproval?: () => void
}

/**
 * 渲染待审批横幅；没有待审批时返回 `null`。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 待审批数据面；缺省走 Tauri。
 * @param props.titleOf - 会话标题解析。
 * @param props.openSession - 会话切换。
 * @param props.focusApproval - 聚焦审批卡。
 * @returns 横幅或空。
 */
export function ApprovalBanner({
  t,
  wire = DEFAULT_WIRE,
  titleOf = (sessionId) => sessionId,
  openSession = () => {},
  focusApproval = () => {},
}: ApprovalBannerProps): ReactNode {
  const [snapshot, setSnapshot] = useState<PendingApprovalsSnapshotWire | null>(null)

  useEffect(() => {
    let alive = true
    const apply = (next: PendingApprovalsSnapshotWire): void => {
      if (alive) setSnapshot(next)
    }
    // 先订阅再查询：中间新插入的请求不会既不在查询结果里、也收不到事件。
    const off = wire.subscribe(apply)
    void wire.pending().then(apply).catch((reason: unknown) => {
      console.warn('[SHANIU] 待审批横幅读取快照失败', reason)
    })
    return () => {
      alive = false
      off()
    }
  }, [wire])

  if (snapshot === null || snapshot.approvals.length === 0) return null

  const count = snapshot.approvals.length
  const rows = snapshot.approvals.slice(0, MAX_ROWS)
  const rest = count - rows.length
  const ttlSeconds = Math.max(1, Math.round(snapshot.ttlMs / 1000))

  return (
    <section
      className="shaniu-approval-banner"
      role="status"
      aria-live="polite"
      aria-label={t('banner.title', { count })}
      data-attended={snapshot.attended ? 'true' : 'false'}
    >
      <div className="shaniu-approval-banner-head">
        <strong className="shaniu-approval-banner-title">{t('banner.title', { count })}</strong>
        <span className="shaniu-approval-banner-hint">
          {snapshot.attended ? t('banner.hint') : t('banner.unattended', { ttl: ttlSeconds })}
        </span>
      </div>
      <ul className="shaniu-approval-banner-list">
        {rows.map((view) => (
          <li key={view.requestId}>
            <button
              type="button"
              className="shaniu-approval-banner-row"
              title={`${titleOf(view.sessionId)} · ${t('banner.tool', { tool: view.tool })}`}
              onClick={() => {
                // 顺序固定：先切会话（卡只会出现在那个会话里），再聚焦。
                openSession(view.sessionId)
                focusApproval()
              }}
            >
              <span className="shaniu-approval-banner-session">{titleOf(view.sessionId)}</span>
              <span className="shaniu-approval-banner-tool">{t('banner.tool', { tool: view.tool })}</span>
              <span className="shaniu-approval-banner-action">{t('banner.open')}</span>
            </button>
          </li>
        ))}
        {rest > 0 ? (
          <li className="shaniu-approval-banner-more">{t('banner.more', { count: rest })}</li>
        ) : null}
      </ul>
    </section>
  )
}
