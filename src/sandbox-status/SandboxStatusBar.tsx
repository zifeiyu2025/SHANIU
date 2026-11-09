/**
 * SH-021 沙箱状态条：`shell.bottom` 的常驻占用者。
 *
 * 为什么是底部状态条：AGENTS §11.3 要求「不静默降级」，而这条红线的隐含前提是
 * **用户随时看得见**。设置页里的状态只在打开设置时可见，侧栏/会话头又只在会话内可见；
 * `shell.bottom` 是 DSH 自带的整行常驻槽位（AppFrame 直接渲染，无会话也有），
 * 不新造外壳、不挤占会话区，是最贴合上游信息架构的落位。
 *
 * 职责边界（重要）：这里**只展示，不操作**。网络授权按会话生效，而根作用域槽位
 * 拿不到"当前会话"——`retainedBy.mainView` 指向的是仍被保留的那一条，用户开一个
 * 新草稿时它会落在上一条历史会话上（实测：批准按钮会把授权记到昨天的会话头上）。
 * 因此会话级的批准/撤销放在 `conversation.session.header.utilities`（见 NetworkToggle），
 * 本组件只负责常驻状态与"哪些会话已放行"。
 *
 * 三件事必须一眼看到：
 *  1. 当前模式（read_only / workspace_write / danger_full_access）与 OS 机制；
 *  2. 容器 profile 名（OS 模式下才有）；
 *  3. 网络是断还是已放行，放行给了哪些会话。
 *
 * 逻辑沙箱（`os_enabled=false`）整条变成警示底色 + 左侧粗边 + 警告图标 + 显眼文字，
 * 与 OS 沙箱态在视觉上**不能**长得一样——这正是 §11.3 的可见性要求。
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-layout/client'
import {
  Button,
  IconRefreshOutlineRegular,
  IconShieldOutlineRegular,
  IconWarningTriangleOutlineRegular,
  Tag,
} from '@deepseek-ai/dsh-client-ui-primitives'
import { NS } from './locales'
import { refreshGrants, useSandboxState } from './state'
import {
  hostSandboxWire,
  type OsSandboxWire,
  type SandboxModeWire,
  type SandboxStatusWire,
  type SandboxWire,
} from './wire'
import './sandbox-status.css'

/** 桌面运行时的默认数据面（模块级常量：保持引用稳定，避免 effect 每次重跑）。 */
const DEFAULT_WIRE = hostSandboxWire()

/** 状态条拿到的 props：根部标准套件 + 本命名空间词典 + 可注入的数据面（测试用）。 */
type SandboxStatusBarProps = PropsRuntime<'shell.bottom'> & PropsLocale<typeof NS> & {
  readonly wire?: SandboxWire
}

/** 策略 → 词典键（显式映射：`t` 的键类型不接受运行时拼接的字符串）。 */
function modeKey(mode: SandboxModeWire): 'mode.read_only' | 'mode.workspace_write' | 'mode.danger_full_access' {
  switch (mode) {
    case 'read_only': return 'mode.read_only'
    case 'workspace_write': return 'mode.workspace_write'
    case 'danger_full_access': return 'mode.danger_full_access'
  }
}

/** OS 机制 → 词典键。 */
function osKey(os: OsSandboxWire): 'os.app_container' | 'os.logical' {
  return os === 'app_container' ? 'os.app_container' : 'os.logical'
}

/** 会话 id 的短形：审计里也是全 id，这里只求「看得出是哪一条」。 */
function shortId(id: string): string {
  return id.length > 12 ? `${id.slice(0, 12)}…` : id
}

/**
 * 渲染常驻沙箱状态条。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 宿主命令实现；缺省走 Tauri。
 * @returns 状态条。
 */
export function SandboxStatusBar({ t, wire = DEFAULT_WIRE }: SandboxStatusBarProps): ReactNode {
  const { grants, notice } = useSandboxState()
  const [status, setStatus] = useState<SandboxStatusWire | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await wire.status())
      setLoadError(null)
    } catch (error: unknown) {
      setLoadError(String(error))
    }
  }, [wire])

  const refresh = useCallback(() => {
    void refreshStatus()
    void refreshGrants(wire)
  }, [refreshStatus, wire])

  useEffect(() => { refresh() }, [refresh])

  const logical = status !== null && !status.os_enabled
  // 逻辑沙箱没有 OS 级网络拦截：授权记录仍在，但实际可达性不变，不能显示「已断/已放行」。
  const networkText = logical
    ? t('network.notEffective')
    : grants.length === 0
      ? t('network.denied')
      : t('network.grantedSessions', { sessions: grants.map((grant) => shortId(grant.session_id)).join('、') })

  return (
    <div
      className="shaniu-sandbox-bar"
      data-os={status === null ? 'unknown' : status.os_enabled ? 'os' : 'logical'}
      role="status"
      aria-live="polite"
    >
      <div className="shaniu-sandbox-bar-main">
        <span className="shaniu-sandbox-bar-group">
          <IconShieldOutlineRegular />
          <span className="shaniu-sandbox-bar-label">{t('label.sandbox')}</span>
          {status === null
            ? <span className="shaniu-sandbox-bar-muted">{loadError ?? '…'}</span>
            : (
              <>
                {/* 档位是主：先给出用户选的那一档，机制与 profile 是实现细节，排在后面。 */}
                <Tag tone={status.mode === 'danger_full_access' ? 'danger' : 'neutral'}>
                  {t(modeKey(status.mode))}
                </Tag>
                <Tag tone={status.enforcement === 'full' ? 'success' : 'warning'}>
                  <span title={t(status.enforcement === 'full' ? 'enforcement.fullHint' : 'enforcement.partialHint')}>
                    {t(status.enforcement === 'full' ? 'enforcement.full' : 'enforcement.partial')}
                  </span>
                </Tag>
                <span className="shaniu-sandbox-bar-mechanism">
                  <span className="shaniu-sandbox-bar-label">{t('label.mechanism')}</span>
                  <Tag tone={status.os_enabled ? 'success' : 'warning'}>
                    {t(osKey(status.os))}
                  </Tag>
                  {status.profile !== '' && (
                    <span className="shaniu-sandbox-bar-profile" title={t('profile', { profile: status.profile })}>
                      {status.profile}
                    </span>
                  )}
                </span>
              </>
            )}
        </span>

        {logical && (
          <span className="shaniu-sandbox-bar-warning">
            <IconWarningTriangleOutlineRegular />
            {t('os.logicalWarning')}
          </span>
        )}

        <span className="shaniu-sandbox-bar-group shaniu-sandbox-bar-network">
          <span className="shaniu-sandbox-bar-label">{t('label.network')}</span>
          <span className="shaniu-sandbox-bar-network-state">{networkText}</span>
          <Button
            size="sm"
            variant="ghost"
            aria-label={t('refresh')}
            title={t('refresh')}
            onClick={refresh}
          >
            <IconRefreshOutlineRegular />
          </Button>
        </span>
      </div>

      {(notice !== null || loadError !== null) && (
        <div className="shaniu-sandbox-bar-notes">
          {notice !== null && <span data-tone={notice.tone}>{notice.text}</span>}
          {loadError !== null && <span data-tone="error">{t('error.status', { error: loadError })}</span>}
        </div>
      )}
    </div>
  )
}
