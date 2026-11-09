/**
 * SH-021 沙箱状态条：`shell.bottom` 的常驻占用者。
 *
 * 为什么是底部状态条：AGENTS §11.3 要求「不静默降级」，而这条红线的隐含前提是
 * **用户随时看得见**。设置页里的状态只在打开设置时可见，侧栏/会话头又只在会话内可见；
 * `shell.bottom` 是 DSH 自带的整行常驻槽位（AppFrame 直接渲染，无会话也有），
 * 不新造外壳、不挤占会话区，是最贴合上游信息架构的落位。
 *
 * 职责边界（重要）：这里**只展示，不操作**。
 *
 * 三件事必须一眼看到：
 *  1. 当前模式（read_only / workspace_write / danger_full_access）；
 *  2. 强制完整度（full / partial）——与工具结果里回填给模型的是同一判定；
 *  3. 机制与容器 profile 名（降权展示：档位才是用户能理解的那一层）。
 *
 * **不显示网络**（2026-11-10 定，对齐 DSH）：网络不是策略维度，容器一律可出网，
 * 一条恒为「可联网」的状态既不能反映档位、也不能反映风险，留着只会误导。
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

/**
 * 渲染常驻沙箱状态条。
 * @param props.t - 本命名空间词典。
 * @param props.wire - 宿主命令实现；缺省走 Tauri。
 * @returns 状态条。
 */
export function SandboxStatusBar({ t, wire = DEFAULT_WIRE }: SandboxStatusBarProps): ReactNode {
  const [status, setStatus] = useState<SandboxStatusWire | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      setStatus(await wire.status())
      setLoadError(null)
    } catch (error: unknown) {
      setLoadError(String(error))
    }
  }, [wire])

  useEffect(() => { void refresh() }, [refresh])

  const logical = status !== null && !status.os_enabled

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

        <span className="shaniu-sandbox-bar-group shaniu-sandbox-bar-actions">
          <Button
            size="sm"
            variant="ghost"
            aria-label={t('refresh')}
            title={t('refresh')}
            onClick={() => void refresh()}
          >
            <IconRefreshOutlineRegular />
          </Button>
        </span>
      </div>

      {loadError !== null && (
        <div className="shaniu-sandbox-bar-notes">
          <span data-tone="error">{t('error.status', { error: loadError })}</span>
        </div>
      )}
    </div>
  )
}
