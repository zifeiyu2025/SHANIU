import type { ReactNode } from 'react'
import type { InjectFace, PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import {
  IconChevronDownOutlineRegular,
  IconDataOutlineRegular,
  StateDot,
} from '@deepseek-ai/dsh-client-ui-primitives'
import type {} from './locales'
import './model-seat.css'

type ModelConfigSeatProps =
  & PropsRuntime<'conversation.input.model'>
  & PropsLocale<'shaniuModelConfig'>
  & InjectFace<{ open: () => void, loading: boolean, failed: boolean }>

/**
 * 未配置任何提供商时的模型席位。
 *
 * 结构与视觉刻意对齐 DSH `ModelSelect` 的 trigger（同槽位原件）：
 * 28px 高、13px/20px 字重 400、左侧模型图标、右侧 chevron、透明底，
 * 与同一行的访问模式 chip 同规格（见 model-seat.css 的取值出处）。
 * 空态文案沿用上游：「请选择模型」「正在加载模型…」。
 *
 * 失败态单独一档：读取提供商目录 reject 时显示「读取模型失败」并换错误圆点，
 * 与加载态在文案、圆点、`data-*` 上都不同——两种状态不能长得一样。
 */
export function ModelConfigSeat({ open, loading, failed, t }: ModelConfigSeatProps): ReactNode {
  const label = loading ? t('seat.loading') : failed ? t('seat.failed') : t('seat.empty')
  return (
    <button
      type="button"
      className="shaniu-model-seat"
      aria-label={t('seat.aria')}
      aria-haspopup="dialog"
      title={label}
      aria-busy={loading ? true : undefined}
      data-loading={loading ? '' : undefined}
      data-failed={failed ? '' : undefined}
      onClick={() => { open() }}
    >
      <IconDataOutlineRegular className="shaniu-model-seat-icon" size={16} />
      <span className="shaniu-model-seat-label">{label}</span>
      {loading
        ? <StateDot state="ongoing" />
        : failed
          ? <StateDot state="error" />
          : <IconChevronDownOutlineRegular className="shaniu-model-seat-chevron" />}
    </button>
  )
}
