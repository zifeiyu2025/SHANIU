/**
 * 宿主命令 → 客户端动作的触发总线（SH-115）。
 *
 * `/export` 的语义是"对当前会话发起日志下载"——真正知道怎么弹对话框、怎么调
 * `export_probe`/`export_session_log` 的代码在 `header-actions`（SH-114 批 2 挂的
 * `SessionLogDownloadController`，连同它的 Dialog 挂载与词典）。命令面在 adapter
 * （transport 层），不该也不会去重建第二条导出链——这里给双方一个 8 行的接缝：
 * header-actions 注册执行者，adapter 触发。注册缺席（插件还没装配）如实报错，
 * 不伪造"已请求"。
 *
 * 单执行者就够：下载入口只有会话头菜单与 `/export` 两条，都汇聚到同一个
 * controller 实例（并发请求会被它合并进同一次操作）。
 */

import type { SessionId } from '@deepseek-ai/dsh-session/types'

export type ExportTrigger = (sessionId: SessionId) => Promise<void> | void

let trigger: ExportTrigger | undefined

/**
 * 注册 `/export` 的执行者（header-actions 在创建 downloadController 后调用）。
 * @returns 注销函数（插件生命周期结束时调用，避免触达已 dispose 的 controller）。
 */
export function registerExportTrigger(next: ExportTrigger): () => void {
  trigger = next
  return () => {
    if (trigger === next) trigger = undefined
  }
}

/**
 * 触发一次会话日志下载。
 * @returns 是否有执行者接单（false = 导出插件未装配，调用方回错误结果）。
 */
export function requestExport(sessionId: SessionId): boolean {
  if (trigger === undefined) return false
  void trigger(sessionId)
  return true
}
