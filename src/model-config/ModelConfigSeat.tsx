import type { ReactNode } from 'react'
import type { InjectFace, PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import { Button } from '@deepseek-ai/dsh-client-ui-primitives'
import type {} from './locales'

type ModelConfigSeatProps =
  & PropsRuntime<'conversation.input.model'>
  & PropsLocale<'shaniuModelConfig'>
  & InjectFace<{ open: () => void }>

/**
 * 未配置任何提供商时的模型席位。
 *
 * 官方在同位置显示「请选择模型」并展开一个空菜单；这里按产品口径改成
 * 「模型配置」，点击直接打开设置里的模型页。
 */
export function ModelConfigSeat({ open, t }: ModelConfigSeatProps): ReactNode {
  const label = t('seat.label')
  return (
    <Button size="sm" variant="ghost" aria-label={t('seat.aria')} onClick={() => { open() }}>
      {label}
    </Button>
  )
}
