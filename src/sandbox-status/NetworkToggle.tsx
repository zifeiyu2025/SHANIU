/**
 * 会话级网络开关：`conversation.session.header.utilities` 的一颗按钮。
 *
 * 为什么放在会话头部而不是底部状态条：网络授权的作用域是**会话**，而这个槽位
 * 拿到的是权威的 `sessionId`。根作用域只能用 `retainedBy.mainView` 猜"当前会话"，
 * 在"新建草稿"状态下会猜到上一条历史会话（实测踩过），那样批准就会落到用户
 * 没在看的会话头上——比没有入口更糟。
 *
 * 按钮本身只做两件事：批准 / 撤销一次会话级授权，并把结果写进共享状态，
 * 让底部状态条同一刻跟着变（两边看到同一份授权列表）。
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-conversation/client'
import { Button, IconGlobeOutlineRegular, Tooltip } from '@deepseek-ai/dsh-client-ui-primitives'
import { NS } from './locales'
import { refreshGrants, setNotice, useSandboxState } from './state'
import { hostSandboxWire, type SandboxWire } from './wire'

/** 桌面运行时的默认数据面（引用稳定，避免 effect 每次重跑）。 */
const DEFAULT_WIRE = hostSandboxWire()

/** 按钮拿到的 props：会话槽位标准套件 + 本命名空间词典 + 可注入的数据面（测试用）。 */
type NetworkToggleProps = PropsRuntime<'conversation.session.header.utilities'>
  & PropsLocale<typeof NS>
  & { readonly wire?: SandboxWire }

/**
 * 渲染会话头部的网络开关。
 * @param props.sessionId - 当前会话 id（槽位注入，权威来源）。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 宿主命令实现；缺省走 Tauri。
 * @returns 一颗带提示的图标按钮。
 */
export function NetworkToggle({ sessionId, t, wire = DEFAULT_WIRE }: NetworkToggleProps): ReactNode {
  const { grants } = useSandboxState()
  const [pending, setPending] = useState(false)
  const granted = grants.some((grant) => grant.session_id === String(sessionId))

  /**
   * 逻辑沙箱下没有 OS 级网络拦截，放行与撤销都不改变实际可达性：
   * 这里必须显示「未生效」并禁用，而不是让用户以为点一下就能开通（Q4）。
   * 读取失败保持原来的可用状态——失败提示由常驻状态条负责，不在这里重复。
   */
  const [osEnabled, setOsEnabled] = useState<boolean | null>(null)
  useEffect(() => {
    let live = true
    wire.status().then(
      (next) => { if (live) setOsEnabled(next.os_enabled) },
      () => { if (live) setOsEnabled(null) },
    )
    return () => { live = false }
  }, [wire])
  const notEffective = osEnabled === false

  const toggle = useCallback(async () => {
    setPending(true)
    setNotice(null)
    try {
      if (granted) {
        const removed = await wire.revoke(sessionId)
        setNotice({
          tone: removed ? 'success' : 'error',
          text: t(removed ? 'feedback.revoked' : 'feedback.notGranted'),
        })
      } else {
        await wire.grant(sessionId)
        setNotice({ tone: 'success', text: t('feedback.granted') })
      }
      await refreshGrants(wire)
    } catch (error: unknown) {
      setNotice({
        tone: 'error',
        text: t(granted ? 'feedback.revokeFailed' : 'feedback.grantFailed', { error: String(error) }),
      })
    } finally {
      setPending(false)
    }
  }, [granted, sessionId, t, wire])

  const label = notEffective
    ? t('network.notEffective')
    : pending
      ? t(granted ? 'action.revoking' : 'action.granting')
      : t(granted ? 'action.revokeTooltip' : 'action.grantTooltip')

  return (
    <Tooltip label={label} side="bottom" align="end" portal>
      <Button
        size="sm"
        variant={granted ? 'outline' : 'ghost'}
        aria-label={label}
        aria-pressed={granted}
        data-granted={granted ? 'true' : 'false'}
        data-effective={notEffective ? 'false' : 'true'}
        disabled={pending || notEffective}
        icon={<IconGlobeOutlineRegular />}
        onClick={() => { void toggle() }}
      />
    </Tooltip>
  )
}
