/** `shaniuApproval` 命名空间：全局待审批横幅文案。 */

/** 本插件独占的词典命名空间。 */
export const NS = 'shaniuApproval'

/**
 * 简体中文词典（键集的事实来源）。
 *
 * 纪律：横幅只说**待办**，不重复审批卡里的风险措辞；也不出现后端字段名
 * （`attended` / `revision` 是协议词）。超时口径只提「按拒绝收口」，
 * 因为那是用户能观察到的结果，而不是实现细节。
 */
export const zh = {
  'banner.title': '{count} 个请求等你审批',
  'banner.hint': '点一行直接跳到那张审批卡。',
  'banner.unattended': '接收页面的心跳已中断；{ttl} 秒内没恢复就会按拒绝收口。',
  'banner.tool': '工具：{tool}',
  'banner.open': '去处理',
  'banner.more': '还有 {count} 个',
} as const

/** 词典键联合。 */
export type ApprovalBannerKey = keyof typeof zh

/** 英文词典，键集与中文对齐。 */
export const en: Record<ApprovalBannerKey, string> = {
  'banner.title': '{count} requests awaiting approval',
  'banner.hint': 'Click a row to jump straight to that approval card.',
  'banner.unattended': 'The receiving page stopped heartbeating; without recovery it will be denied in {ttl}s.',
  'banner.tool': 'Tool: {tool}',
  'banner.open': 'Review',
  'banner.more': '{count} more',
}

declare module '@deepseek-ai/dsh-client-ui-slots' {
  interface LocaleNamespaceMap {
    /** 全局待审批横幅的全部文案。 */
    shaniuApproval: ApprovalBannerKey
  }
}
