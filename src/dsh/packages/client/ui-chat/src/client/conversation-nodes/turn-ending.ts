import type { TurnEndReason } from '@deepseek-ai/dsh-session/types'
import { displayFailure } from './event-projection.ts'

/**
 * 一个回合的结局口径（真机冒烟 2026-11-17 N3）。
 *
 * 同一个截断回合曾同屏三种说法：行首红字「本轮运行失败」、正文「被截断」、回合组标题
 * 「已完成，用时 N」。根因是三处各自读 `reason.kind` 各判一次，谁都可能漏掉一支。
 * 这里给出**唯一**判定：截断既不是失败、也不是已完成，而是一个独立结局；
 * 行首（`turn-error` / `turn-max-tokens`）、组标题（`TurnProcessNodeView`）都只读它。
 */
export type TurnEndingKind =
  /** 正常完成（含 blocked / forked / 插件合并等没有结论文案的原因）。 */
  | 'completed'
  /** 达到输出上限被截断：部分完成，不是失败。 */
  | 'truncated'
  /** 用户 / 父会话取消，或恢复对账补写的「进程中途退出」。 */
  | 'stopped'
  /** 有结构化失败记录的回合。 */
  | 'failed'
  /** 档案没有收口记录，宿主也给不出结论。 */
  | 'unknown'

/** 结局的判定结果；`code` 是稳定机器标记，文案由各处词典按语境取。 */
export interface TurnEnding {
  readonly kind: TurnEndingKind
  readonly code?: string
  /** 结局正文；正常完成、纯用户取消与截断没有（截断的说明由 max-tokens 提示行负责）。 */
  readonly message?: string
  readonly detail?: string
}

/**
 * 判定一个回合的结局。所有「这个回合算成功/失败/截断/未知」的问题都只问这里。
 * @param reason - 回合的 `turn/end` 原因；`undefined` 表示没有收口记录但已按完成处理。
 * @returns 唯一结局口径。
 */
export function turnEnding(reason: TurnEndReason | undefined): TurnEnding {
  if (reason === undefined) return { kind: 'completed' }
  switch (reason.kind) {
    case 'error':
      return { kind: 'failed', ...displayFailure(reason.error) }
    case 'aborted':
      // 用户与父会话取消只说「已停止」；退出登录要单独说明原因。
      return reason.reason.kind === 'hook' && reason.reason.reason === 'deepseek-account/signed-out'
        ? { kind: 'stopped', code: 'ACCOUNT_SIGNED_OUT', message: 'Stopped because you signed out of DeepSeek.' }
        : { kind: 'stopped' }
    case 'interrupted':
      // SHANIU：宿主恢复对账补写的结论——进程在回合中途退出（有证据的没写完）。
      return { kind: 'stopped', code: 'INTERRUPTED', message: '进程在这一轮结束前退出了，回答可能不完整。' }
    case 'max-tokens':
      // 截断是「没写完」，不是失败；说明交给 turn-max-tokens 提示行，避免同回合两条结局行。
      return { kind: 'truncated', code: 'MAX_TOKENS' }
    case 'indeterminate':
      // SHANIU：档案没有收口记录，不知道这一轮是结束了还是还在跑。
      return { kind: 'unknown', code: 'TURN_UNKNOWN', message: 'This turn has no recorded ending.' }
    default:
      return { kind: 'completed' }
  }
}

/**
 * 行首结局行（`turn-error`）该不该出现：只有带文案的结局才写一行。
 * 截断归 `turn-max-tokens` 提示行、纯用户取消与正常完成没有结局行。
 * @param ending - `turnEnding` 的判定结果。
 * @returns 要写进 `TurnErrorNode` 的字段，或 undefined 表示不写行。
 */
export function turnEndingRow(
  ending: TurnEnding,
): { readonly message: string; readonly code?: string; readonly detail?: string } | undefined {
  if (ending.kind === 'truncated' || ending.message === undefined) return undefined
  return {
    message: ending.message,
    ...(ending.code === undefined ? {} : { code: ending.code }),
    ...(ending.detail === undefined ? {} : { detail: ending.detail }),
  }
}
