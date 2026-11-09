/**
 * SH-063 MCP 连接器：注册在插件页 `plugins.item` 的宿主条目。
 *
 * 外壳（卡片、列表项、详情页、返回、面包屑、tab 容器）全部由插件页提供；
 * 这里只按 `view` 契约渲染 `summary`（卡片摘要行）与 `page`（详情页主体）。
 * 详情页主体用上游基础组件拼装：`SegmentedTabs` / `DisclosureRow` / `Tag` /
 * `Button` / `Checkbox` / `StateDot` / `PathLabel` / `RiskConfirmation` /
 * `TextShimmer`，不自造卡片与容器样式。
 *
 * 数据面：`src/transport/mcp-wire.ts` 的宿主命令与 `shaniu://mcp` 事件。
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import type { PluginConfigViewProps } from '@deepseek-ai/dsh-client-ui-plugin-manager/client'
import {
  Button,
  Checkbox,
  DisclosureRow,
  IconLinkOutlineRegular,
  PathLabel,
  RiskConfirmation,
  SegmentedTabs,
  StateDot,
  Tag,
  TextShimmer,
  type TagTone,
} from '@deepseek-ai/dsh-client-ui-primitives'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import { tauriBridge } from '../transport/tauri'
import {
  mcpConnectAndReview,
  mcpListExemptions,
  mcpListServers,
  mcpResolveTrust,
  mcpRevokeExemption,
  mcpRevokeTrust,
  tauriMcpSource,
  type ExemptionWire,
  type McpRiskLevel,
  type McpServerSummaryWire,
  type ReviewPayloadWire,
  type ReviewToolWire,
} from '../transport/mcp-wire'
import { ConfigReadOnlyNotice, useConfigReadOnly } from '../config-readonly/ConfigReadOnly'
import { NS } from './locales'
import {
  approvedCount,
  buildDecisions,
  defaultSelections,
  needsInjectionConsent,
  SELECTABLE_RISKS,
  shortFingerprint,
  summarizeServers,
  type ToolSelection,
} from './model'
import './mcp-trust.css'

/**
 * 本条目从槽位拿到的 props：`view` 契约（`plugins.item` 才有）+ 本命名空间词典。
 *
 * `settings.section` 的临时挂载点不传 `view`（只给 `close`），所以这里放开为
 * 可选：两个挂载点共用**同一个组件定义、同一份数据源与状态**，不是两份 UI。
 */
type McpConnectorProps = Partial<PluginConfigViewProps> & PropsLocale<typeof NS>

/** 详情页内的两个视图。 */
type ConnectorTab = 'servers' | 'exemptions'

/** 需要二次确认的动作（`RiskConfirmation` 是唯一的确认入口）。 */
type Confirmation =
  | { readonly kind: 'approve'; readonly serverId: string }
  | { readonly kind: 'retrust'; readonly serverId: string; readonly payload: ReviewPayloadWire }

/** 服务器状态 → 可见 Tag 的色调（文字本身仍由词典给出，不靠颜色传达）。 */
function statusTone(status: string): TagTone {
  switch (status) {
    case 'trusted': return 'success'
    case 'review': return 'warning'
    case 'retrust': return 'danger'
    case 'blocked': return 'danger'
    case 'failed': return 'danger'
    case 'degraded': return 'warning'
    case 'disabled': return 'quiet'
    default: return 'neutral'
  }
}

/** 状态 → `StateDot` 的形状（仅在需要注意力时使用圆点）。 */
function statusDot(status: string): 'done' | 'warning' | 'ongoing' | 'error' | 'idle' {
  switch (status) {
    case 'trusted': return 'done'
    case 'review': return 'warning'
    case 'retrust': return 'error'
    case 'blocked': return 'error'
    case 'failed': return 'error'
    case 'degraded': return 'warning'
    case 'idle': return 'ongoing'
    default: return 'idle'
  }
}

/** 状态 → 一行文字说明（「待审查」与「需要重新信任」必须在文字上就不同）。 */
function stateKey(status: string):
  | 'state.review' | 'state.retrust' | 'state.trusted' | 'state.blocked'
  | 'state.failed' | 'state.degraded' | 'state.idle' | 'state.disabled' {
  switch (status) {
    case 'review': return 'state.review'
    case 'retrust': return 'state.retrust'
    case 'trusted': return 'state.trusted'
    case 'blocked': return 'state.blocked'
    case 'failed': return 'state.failed'
    case 'degraded': return 'state.degraded'
    case 'disabled': return 'state.disabled'
    default: return 'state.idle'
  }
}

/** 状态 → `status.*` 词典键。 */
function statusKey(status: string):
  | 'status.disabled' | 'status.idle' | 'status.review' | 'status.trusted'
  | 'status.blocked' | 'status.retrust' | 'status.degraded' | 'status.failed' {
  switch (status) {
    case 'disabled': return 'status.disabled'
    case 'review': return 'status.review'
    case 'trusted': return 'status.trusted'
    case 'blocked': return 'status.blocked'
    case 'retrust': return 'status.retrust'
    case 'degraded': return 'status.degraded'
    case 'failed': return 'status.failed'
    default: return 'status.idle'
  }
}

/** 风险档位 → `risk.*` 词典键。 */
function riskKey(risk: McpRiskLevel):
  | 'risk.read' | 'risk.write_in_workspace' | 'risk.execute'
  | 'risk.destructive' | 'risk.plan_review' {
  switch (risk) {
    case 'read': return 'risk.read'
    case 'write_in_workspace': return 'risk.write_in_workspace'
    case 'execute': return 'risk.execute'
    case 'plan_review': return 'risk.plan_review'
    default: return 'risk.destructive'
  }
}

/** 错误对象 → 可读消息（不含任何令牌或环境变量明文）。 */
function messageOf(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason)
}

/** 时间戳 → 本地时间；0 表示没有记录。 */
function formatTime(ms: number | undefined): string {
  if (ms === undefined || ms === 0) return '—'
  return new Date(ms).toLocaleString()
}

/** 服务器列表的唯一数据源（返回摘要视图与详情页共用）。 */
function useServers(tick: number): {
  readonly servers: readonly McpServerSummaryWire[] | null
  readonly error: string | null
} {
  const [servers, setServers] = useState<readonly McpServerSummaryWire[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  useEffect(() => {
    let live = true
    mcpListServers(tauriBridge()).then(
      (next) => {
        if (!live) return
        setServers(next)
        setError(null)
      },
      (reason: unknown) => {
        if (live) setError(messageOf(reason))
      },
    )
    return () => { live = false }
  }, [tick])
  return { servers, error }
}

/** 订阅应用级 MCP 事件，返回一个随事件递增的计数（供数据读取 effect 依赖）。 */
function useMcpTick(): readonly [number, () => void] {
  const [tick, setTick] = useState(0)
  const reload = useCallback(() => { setTick(value => value + 1) }, [])
  useEffect(() => tauriMcpSource().listen(reload), [reload])
  return [tick, reload]
}

/**
 * `plugins.item` 入口：按上游契约分流卡片摘要与详情页主体。
 * @param props - `view`、未使用的 `form` 与本命名空间 `t`。
 * @returns 摘要文本或详情页主体。
 */
export function McpConnector({ view, t }: McpConnectorProps): ReactNode {
  return view === 'summary' ? <McpConnectorSummary t={t} /> : <McpConnectorPage t={t} />
}

/** 卡片摘要行：状态聚合是列表层唯一的位置。 */
function McpConnectorSummary({ t }: { t: McpConnectorProps['t'] }): ReactNode {
  const [tick] = useMcpTick()
  const { servers, error } = useServers(tick)
  if (error !== null && servers === null) return t('summary.unavailable')
  if (servers === null) return t('summary.loading')
  return summarizeServers(servers, t)
}

/** 详情页主体。 */
function McpConnectorPage({ t }: { t: McpConnectorProps['t'] }): ReactNode {
  const [tick, reload] = useMcpTick()
  const { servers, error } = useServers(tick)
  const configReadOnly = useConfigReadOnly()
  const [tab, setTab] = useState<ConnectorTab>('servers')
  const [exemptions, setExemptions] = useState<readonly ExemptionWire[] | null>(null)
  const [openId, setOpenId] = useState<string | null>(null)
  const [reviews, setReviews] = useState<Record<string, ReviewPayloadWire>>({})
  const [selections, setSelections] = useState<Record<string, Record<string, ToolSelection>>>({})
  const [busyId, setBusyId] = useState<string | null>(null)
  const [failure, setFailure] = useState<string | null>(null)
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null)
  const [acknowledged, setAcknowledged] = useState(false)

  useEffect(() => {
    let live = true
    mcpListExemptions(tauriBridge()).then(
      (next) => { if (live) setExemptions(next) },
      (reason: unknown) => {
        if (live) setFailure(messageOf(reason))
      },
    )
    return () => { live = false }
  }, [tick])

  const fingerprints = new Map<string, string | null>(
    (servers ?? []).map(server => [server.id, server.fingerprint]),
  )

  /** 读取一次审查载荷；`acceptNewBinary` 只由重新信任的显式确认传入。 */
  async function connect(serverId: string, acceptNewBinary: boolean): Promise<void> {
    setBusyId(serverId)
    setFailure(null)
    try {
      const payload = await mcpConnectAndReview(tauriBridge(), serverId, acceptNewBinary)
      setReviews(previous => ({ ...previous, [serverId]: payload }))
      setSelections(previous => ({
        ...previous,
        [serverId]: defaultSelections(payload),
      }))
      setOpenId(serverId)
      reload()
    } catch (reason) {
      setFailure(messageOf(reason))
    } finally {
      setBusyId(null)
    }
  }

  /** 提交裁决（`fingerprint` 由后端比对，不一致即拒绝：防 TOCTOU）。 */
  async function resolve(
    serverId: string,
    payload: ReviewPayloadWire,
    decisions: ReturnType<typeof buildDecisions>,
  ): Promise<void> {
    setBusyId(serverId)
    setFailure(null)
    try {
      const next = await mcpResolveTrust(tauriBridge(), serverId, payload.fingerprint, decisions)
      setReviews(previous => ({ ...previous, [serverId]: next }))
      setSelections(previous => ({ ...previous, [serverId]: defaultSelections(next) }))
      reload()
    } catch (reason) {
      setFailure(messageOf(reason))
    } finally {
      setBusyId(null)
    }
  }

  async function revokeTrust(serverId: string): Promise<void> {
    setBusyId(serverId)
    setFailure(null)
    try {
      await mcpRevokeTrust(tauriBridge(), serverId)
      setReviews(previous => {
        const next = { ...previous }
        delete next[serverId]
        return next
      })
      reload()
    } catch (reason) {
      setFailure(messageOf(reason))
    } finally {
      setBusyId(null)
    }
  }

  async function revokeExemption(entry: ExemptionWire): Promise<void> {
    setFailure(null)
    try {
      await mcpRevokeExemption(
        tauriBridge(),
        entry.server_id,
        entry.dispatch_name,
        entry.fingerprint,
      )
      reload()
    } catch (reason) {
      setFailure(messageOf(reason))
    }
  }

  const pending = (servers ?? []).filter(
    server => server.status === 'review' || server.status === 'retrust',
  )
  // blocked 不进 pending（它不需要审查），但**原因必须露在页面上**：
  // 「已阻止」这个词对第一次用的人等于「MCP 坏了」。
  const blocked = (servers ?? []).filter(server => server.status === 'blocked')

  return (
    <div className="mcp-connector">
      <p className="mcp-connector-intro">{t('description')}</p>
      {configReadOnly !== null && (
        <ConfigReadOnlyNotice message={t('configReadOnly', { reason: configReadOnly })} />
      )}
      {failure !== null && (
        <p className="mcp-connector-alert" role="alert">
          <StateDot state="error" />
          {t('actionFailed', { message: failure })}
          <Button size="sm" variant="outline" onClick={reload}>
            {t('refresh')}
          </Button>
        </p>
      )}
      <SegmentedTabs
        items={[
          { value: 'servers', label: t('tab.servers'), id: 'mcp-tab-servers', panelId: 'mcp-panel-servers' },
          { value: 'exemptions', label: t('tab.exemptions'), id: 'mcp-tab-exemptions', panelId: 'mcp-panel-exemptions' },
        ]}
        value={tab}
        onChange={setTab}
        label={t('tabsLabel')}
        size="sm"
      />
      {tab === 'servers'
        ? (
          <div id="mcp-panel-servers" role="tabpanel" aria-labelledby="mcp-tab-servers">
            {servers === null && error === null && (
              <p className="mcp-connector-note" aria-busy="true">
                <TextShimmer active>{t('loading')}</TextShimmer>
              </p>
            )}
            {servers === null && error !== null && (
              <p className="mcp-connector-note" role="alert">
                {t('loadFailed', { message: error })}
              </p>
            )}
            {servers !== null && servers.length === 0 && (
              <p className="mcp-connector-note">{t('empty')}</p>
            )}
            {blocked.length > 0 && (
              <section
                className="mcp-connector-attention mcp-connector-blocked"
                aria-label={t('blocked.title', { count: blocked.length })}
              >
                <p className="mcp-connector-attention-title">
                  <StateDot state="error" />
                  <strong>{t('blocked.title', { count: blocked.length })}</strong>
                </p>
                <ul className="mcp-connector-attention-list">
                  {blocked.map(server => (
                    <li key={server.id}>
                      <Tag tone={statusTone(server.status)}>{t(statusKey(server.status))}</Tag>
                      <span className="mcp-connector-attention-id">{server.id}</span>
                      <span className="mcp-connector-blocked-reason">
                        {server.reason ?? t('blocked.noReason')}
                      </span>
                    </li>
                  ))}
                </ul>
                <p className="mcp-connector-note">{t('blocked.hint')}</p>
              </section>
            )}
            {pending.length > 0 && (
              <section className="mcp-connector-attention" aria-label={t('attention.title', { count: pending.length })}>
                <p className="mcp-connector-attention-title">
                  <StateDot state="warning" />
                  <strong>{t('attention.title', { count: pending.length })}</strong>
                </p>
                <p className="mcp-connector-note">{t('attention.hint')}</p>
                <ul className="mcp-connector-attention-list">
                  {pending.map(server => (
                    <li key={server.id}>
                      <Tag tone={statusTone(server.status)}>{t(statusKey(server.status))}</Tag>
                      <span className="mcp-connector-attention-id">{server.id}</span>
                      <Button
                        size="sm"
                        variant="outline"
                        disabled={busyId !== null || !server.enabled}
                        onClick={() => { void connect(server.id, false) }}
                      >
                        {server.status === 'retrust' ? t('reviewBinary') : t('review')}
                      </Button>
                    </li>
                  ))}
                </ul>
              </section>
            )}
            {servers !== null && servers.length > 0 && (
              <ul className="mcp-connector-servers">
                {servers.map(server => {
                  const open = openId === server.id
                  const review = reviews[server.id]
                  const busy = busyId === server.id
                  const selection = selections[server.id] ?? {}
                  const approved = approvedCount(selection)
                  return (
                    <li key={server.id} className="mcp-connector-server">
                      <DisclosureRow
                        icon={<IconLinkOutlineRegular />}
                        title={server.id}
                        open={open}
                        expandable
                        expandOnRowClick
                        running={busy}
                        onToggle={() => { setOpenId(open ? null : server.id) }}
                        collapsedContent={(
                          <>
                            <Tag tone={statusTone(server.status)}>{t(statusKey(server.status))}</Tag>
                            <span className="mcp-connector-row-note">
                              {t('approvedToolCount', { count: server.approved_tools.length })}
                              {' · '}
                              {t('exemptionCount', { count: server.exemption_count })}
                            </span>
                            <code className="mcp-connector-mono mcp-connector-row-command">
                              {server.command} {server.args.join(' ')}
                            </code>
                          </>
                        )}
                      >
                        <div className="mcp-connector-detail">
                          <p className="mcp-connector-state">{t(stateKey(server.status))}</p>
                          {server.reason !== null && (
                            <p className="mcp-connector-note">{server.reason}</p>
                          )}
                          <dl className="mcp-connector-identity">
                            <dt>{t('state')}</dt>
                            <dd><Tag tone={statusTone(server.status)}>{t(statusKey(server.status))}</Tag></dd>
                            <dt>{t('executable')}</dt>
                            <dd><PathLabel path={server.command} /></dd>
                            <dt>{t('argv')}</dt>
                            <dd className="mcp-connector-mono">{server.args.length > 0 ? server.args.join(' ') : '—'}</dd>
                            <dt>{t('fingerprint')}</dt>
                            <dd className="mcp-connector-mono">
                              {server.fingerprint === null ? '—' : shortFingerprint(server.fingerprint)}
                            </dd>
                            <dt>{t('approvedTools')}</dt>
                            <dd>{t('approvedToolCount', { count: server.approved_tools.length })}</dd>
                            <dt>{t('exemptions')}</dt>
                            <dd>{t('exemptionCount', { count: server.exemption_count })}</dd>
                          </dl>
                          {review !== undefined && review.status === 'retrust'
                            ? (
                              <RetrustPanel
                                t={t}
                                payload={review}
                                busy={busy}
                                onAccept={() => {
                                  setAcknowledged(false)
                                  setConfirmation({ kind: 'retrust', serverId: server.id, payload: review })
                                }}
                                onCollapse={() => { setOpenId(null) }}
                              />
                            )
                            : review !== undefined
                              ? (
                                <ReviewPanel
                                  t={t}
                                  payload={review}
                                  selection={selection}
                                  busy={busy}
                                  approved={approved}
                                  onSelection={(dispatchName, next) => {
                                    setSelections(previous => ({
                                      ...previous,
                                      [server.id]: { ...(previous[server.id] ?? {}), [dispatchName]: next },
                                    }))
                                  }}
                                  onApprove={() => {
                                    setAcknowledged(false)
                                    setConfirmation({ kind: 'approve', serverId: server.id })
                                  }}
                                  onBlock={() => {
                                    void resolve(server.id, review, review.tools.map(tool => ({
                                      dispatch_name: tool.dispatch_name,
                                      approved: false,
                                      risk: 'destructive' as const,
                                    })))
                                  }}
                                  onCollapse={() => { setOpenId(null) }}
                                />
                              )
                              : (
                                <div className="mcp-connector-actions">
                                  {busy && (
                                    <p className="mcp-connector-note mcp-connector-busy" aria-live="polite">
                                      <TextShimmer active>{t('busyHint')}</TextShimmer>
                                    </p>
                                  )}
                                  <Button
                                    size="sm"
                                    variant="primary"
                                    disabled={busy || !server.enabled}
                                    onClick={() => { void connect(server.id, false) }}
                                  >
                                    {server.status === 'retrust'
                                      ? t('reviewBinary')
                                      : server.status === 'review' ? t('review') : t('connect')}
                                  </Button>
                                  {(server.status === 'trusted' || server.status === 'blocked'
                                    || server.status === 'review' || server.status === 'retrust') && (
                                    <Button
                                      size="sm"
                                      variant="outline"
                                      disabled={busy}
                                      onClick={() => { void revokeTrust(server.id) }}
                                    >
                                      {t('revoke')}
                                    </Button>
                                  )}
                                </div>
                              )}
                        </div>
                      </DisclosureRow>
                    </li>
                  )
                })}
              </ul>
            )}
          </div>
        )
        : (
          <ExemptionsPanel
            t={t}
            exemptions={exemptions}
            fingerprints={fingerprints}
            onRevoke={(entry) => { void revokeExemption(entry) }}
          />
        )}
      <RiskConfirmation
        open={confirmation !== null}
        title={confirmation?.kind === 'retrust'
          ? t('confirm.retrustTitle')
          : t('confirm.approveTitle', { id: confirmation?.serverId ?? '' })}
        description={confirmation?.kind === 'retrust'
          ? t('confirm.retrustBody', {
            previous: shortFingerprint(confirmation.payload.previous_executable_sha256 ?? '—'),
            current: shortFingerprint(confirmation.payload.executable_sha256),
          })
          : t('confirm.approveBody', {
            count: confirmation?.kind === 'approve'
              ? approvedCount(selections[confirmation.serverId] ?? {})
              : 0,
            fingerprint: shortFingerprint(reviews[confirmation?.serverId ?? '']?.fingerprint ?? '—'),
          })}
        acknowledgeLabel={confirmation?.kind === 'retrust'
          ? t('confirm.retrustAck')
          : t('confirm.approveAck')}
        cancelLabel={t('confirm.cancel')}
        closeLabel={t('confirm.close')}
        confirmLabel={confirmation?.kind === 'retrust'
          ? t('confirm.retrustAction')
          : t('confirm.approveAction')}
        acknowledged={acknowledged}
        disabled={busyId !== null}
        onAcknowledgedChange={setAcknowledged}
        onCancel={() => { setConfirmation(null) }}
        onConfirm={() => {
          if (confirmation === null) return
          const { kind, serverId } = confirmation
          const payload = reviews[serverId]
          setConfirmation(null)
          if (kind === 'retrust') {
            void connect(serverId, true)
            return
          }
          if (payload === undefined) return
          void resolve(serverId, payload, buildDecisions(payload, selections[serverId] ?? {}))
        }}
      />
    </div>
  )
}

/** 审查面板：逐工具勾选 + 风险档位 + 注入显式启用。 */
function ReviewPanel({
  t, payload, selection, busy, approved, onSelection, onApprove, onBlock, onCollapse,
}: {
  t: McpConnectorProps['t']
  payload: ReviewPayloadWire
  selection: Record<string, ToolSelection>
  busy: boolean
  approved: number
  onSelection: (dispatchName: string, next: ToolSelection) => void
  onApprove: () => void
  onBlock: () => void
  onCollapse: () => void
}): ReactNode {
  return (
    <section className="mcp-connector-panel">
      <p className="mcp-connector-panel-title">
        <StateDot state={statusDot(payload.status)} />
        <strong>{t('tools')}</strong>
        <Tag tone={statusTone(payload.status)}>{t(statusKey(payload.status))}</Tag>
      </p>
      <p className="mcp-connector-note">{t('toolsHint')}</p>
      <dl className="mcp-connector-identity">
        <dt>{t('declarationDigest')}</dt>
        <dd className="mcp-connector-mono">{payload.fingerprint}</dd>
        <dt>{t('executableSha')}</dt>
        <dd className="mcp-connector-mono">{payload.executable_sha256}</dd>
        <dt>{t('coverage')}</dt>
        <dd>
          <Tag tone={payload.coverage?.kind === 'module_tree' ? 'success' : 'quiet'}>
            {t(payload.coverage?.kind === 'module_tree' ? 'coverage.moduleTree' : 'coverage.interpreterOnly')}
          </Tag>
          <span className="mcp-connector-note"> {payload.coverage?.detail ?? t('coverage.interpreterOnlyDetail')}</span>
        </dd>
        {payload.sandbox && (
          <>
            <dt>{t('sandbox')}</dt>
            <dd>
              <Tag
                tone={
                  payload.sandbox.kind === 'app_container'
                    ? 'success'
                    : payload.sandbox.kind === 'logical'
                      ? 'warning'
                      : 'danger'
                }
              >
                {t(
                  payload.sandbox.kind === 'app_container'
                    ? 'sandbox.appContainer'
                    : payload.sandbox.kind === 'logical'
                      ? 'sandbox.logical'
                      : 'sandbox.unsandboxed',
                )}
              </Tag>
              <span className="mcp-connector-note">
                {payload.sandbox.profile ? ` ${payload.sandbox.profile}` : ''}
                {payload.sandbox.workspace
                  ? ` · ${t(payload.sandbox.workspace === 'write' ? 'sandbox.workspaceWrite' : 'sandbox.workspaceRead')}`
                  : ''}
                {(payload.sandbox.allowed_roots?.length ?? 0) > 0
                  ? ` · ${t('sandbox.extraRoots')}：${payload.sandbox.allowed_roots?.join('、')}`
                  : ''}
              </span>
            </dd>
          </>
        )}
        <dt>{t('executable')}</dt>
        <dd><PathLabel path={payload.executable} /></dd>
        <dt>{t('argv')}</dt>
        <dd className="mcp-connector-mono">{payload.argv.length > 0 ? payload.argv.join(' ') : '—'}</dd>
        <dt>{t('cwd')}</dt>
        <dd><PathLabel path={payload.cwd} /></dd>
        <dt>{t('transport')}</dt>
        <dd>{payload.transport}</dd>
        <dt>{t('protocol')}</dt>
        <dd>{payload.protocol_version}</dd>
        <dt>{t('serverName')}</dt>
        <dd>{payload.server_name} ({payload.server_version})</dd>
        <dt>{t('env')}</dt>
        <dd className="mcp-connector-mono">{payload.env.length > 0 ? payload.env.join(' · ') : '—'}</dd>
        <dt>{t('network')}</dt>
        <dd>
          {payload.sandbox?.network === 'not_isolated'
            ? t('sandbox.networkNotIsolated')
            : payload.allow_network
              ? t('allowed')
              : t('notAllowed')}
        </dd>
        <dt>{t('approvedAt')}</dt>
        <dd>{formatTime(payload.approved_at_ms)}</dd>
      </dl>
      {payload.warnings.length > 0 && (
        <div className="mcp-connector-warn">
          <p><Tag tone="warning">{t('warnings')}</Tag></p>
          <ul>
            {payload.warnings.map(warning => <li key={warning}>{warning}</li>)}
          </ul>
        </div>
      )}
      {payload.tools.length === 0 && <p className="mcp-connector-note">{t('noTools')}</p>}
      {payload.tools.length > 0 && (
        <ul className="mcp-connector-tools">
          {payload.tools.map(tool => (
            <ToolRow
              key={tool.dispatch_name}
              t={t}
              tool={tool}
              selection={selection[tool.dispatch_name] ?? {
                approved: false,
                risk: 'destructive',
                enableInjection: false,
              }}
              onChange={next => { onSelection(tool.dispatch_name, next) }}
            />
          ))}
        </ul>
      )}
      {payload.rejected_tools.length > 0 && (
        <div className="mcp-connector-warn">
          <p><Tag tone="danger">{t('rejected')}</Tag></p>
          <ul>
            {payload.rejected_tools.map(tool => (
              <li key={tool.raw_name}><code className="mcp-connector-mono">{tool.raw_name}</code> {tool.reason}</li>
            ))}
          </ul>
        </div>
      )}
      {payload.exemptions.length > 0 && (
        <div className="mcp-connector-warn">
          <p><Tag tone="warning">{t('exemptions')}</Tag></p>
          <ul>
            {payload.exemptions.map(entry => (
              <li key={`${entry.fingerprint}:${entry.dispatch_name}`}>
                <code className="mcp-connector-mono">{entry.dispatch_name}</code>
                {' · '}
                {entry.rule}
                {' · '}
                <span>{entry.hit_fragment}</span>
              </li>
            ))}
          </ul>
        </div>
      )}
      <div className="mcp-connector-actions">
        <Button size="sm" variant="primary" disabled={busy || approved === 0} onClick={onApprove}>
          {t('approve')}
        </Button>
        <Button size="sm" variant="outline" disabled={busy} onClick={onBlock}>
          {t('block')}
        </Button>
        <Button size="sm" variant="ghost" onClick={onCollapse}>
          {t('collapse')}
        </Button>
      </div>
    </section>
  )
}

/** 单个工具：批准位、风险档位、注入命中与显式启用。 */
function ToolRow({
  t, tool, selection, onChange,
}: {
  t: McpConnectorProps['t']
  tool: ReviewToolWire
  selection: ToolSelection
  onChange: (next: ToolSelection) => void
}): ReactNode {
  return (
    <li className="mcp-connector-tool">
      <div className="mcp-connector-tool-head">
        <Checkbox
          checked={selection.approved}
          onChange={approved => { onChange({ ...selection, approved }) }}
          label={tool.raw_name === tool.dispatch_name
            ? tool.dispatch_name
            : `${tool.dispatch_name} (${tool.raw_name})`}
        />
        <select
          className="mcp-connector-risk"
          aria-label={`${t('toolRisk')}: ${tool.dispatch_name}`}
          value={selection.risk}
          disabled={!selection.approved}
          onChange={(event) => {
            onChange({ ...selection, risk: event.target.value as ToolSelection['risk'] })
          }}
        >
          {SELECTABLE_RISKS.map(risk => (
            <option key={risk} value={risk}>{t(riskKey(risk))}</option>
          ))}
        </select>
      </div>
      {tool.description !== '' && <p className="mcp-connector-tool-desc">{tool.description}</p>}
      {tool.server_read_only_hint && <p className="mcp-connector-note">{t('serverHint')}</p>}
      {tool.guard_hits.length > 0 && (
        <div className="mcp-connector-tool-guard">
          <p>
            <Tag tone="danger">{t('injection')}</Tag>
            {tool.guard_exempted && <Tag tone="warning">{t('exempted')}</Tag>}
          </p>
          <ul>
            {tool.guard_hits.map(hit => (
              <li key={`${hit.rule}:${hit.fragment}`}>
                <code className="mcp-connector-mono">{hit.rule}</code>
                {' '}
                <span>{hit.fragment}</span>
              </li>
            ))}
          </ul>
          {tool.guard_explanation !== undefined && (
            <p className="mcp-connector-note">{tool.guard_explanation}</p>
          )}
          {needsInjectionConsent(tool) && (
            <Checkbox
              checked={selection.enableInjection}
              onChange={enableInjection => { onChange({ ...selection, enableInjection }) }}
              label={t('enableInjection')}
              disabled={!selection.approved}
            />
          )}
        </div>
      )}
    </li>
  )
}

/** 重新信任面板：新二进制确认前不启动、不注册。 */
function RetrustPanel({
  t, payload, busy, onAccept, onCollapse,
}: {
  t: McpConnectorProps['t']
  payload: ReviewPayloadWire
  busy: boolean
  onAccept: () => void
  onCollapse: () => void
}): ReactNode {
  return (
    <section className="mcp-connector-panel">
      <p className="mcp-connector-panel-title">
        <StateDot state="error" />
        <strong>{t('retrustTitle')}</strong>
        <Tag tone="danger">{t('status.retrust')}</Tag>
      </p>
      <p className="mcp-connector-note">{t('retrustBody')}</p>
      <dl className="mcp-connector-identity">
        <dt>{t('executable')}</dt>
        <dd><PathLabel path={payload.executable} /></dd>
        <dt>{t('previousSha')}</dt>
        <dd className="mcp-connector-mono">{payload.previous_executable_sha256 ?? '—'}</dd>
        <dt>{t('currentSha')}</dt>
        <dd className="mcp-connector-mono">{payload.executable_sha256}</dd>
        <dt>{t('approvedAt')}</dt>
        <dd>{formatTime(payload.approved_at_ms)}</dd>
        <dt>{t('fileMtime')}</dt>
        <dd>{formatTime(payload.executable_mtime_ms)}</dd>
      </dl>
      {payload.tools.length > 0 && (
        <div className="mcp-connector-warn">
          <p><Tag tone="quiet">{t('staleTools')}</Tag></p>
          <p className="mcp-connector-note">{t('staleToolsHint')}</p>
          <ul>
            {payload.tools.map(tool => (
              <li key={tool.dispatch_name}>
                <code className="mcp-connector-mono">{tool.dispatch_name}</code>
                {' · '}
                {t(riskKey(tool.risk))}
              </li>
            ))}
          </ul>
        </div>
      )}
      <div className="mcp-connector-actions">
        <Button size="sm" variant="primary" disabled={busy} onClick={onAccept}>
          {t('acceptNewBinary')}
        </Button>
        <Button size="sm" variant="ghost" onClick={onCollapse}>
          {t('collapse')}
        </Button>
      </div>
    </section>
  )
}

/** 豁免台账：跨服务器、跨指纹的只读列表 + 撤销。 */
function ExemptionsPanel({
  t, exemptions, fingerprints, onRevoke,
}: {
  t: McpConnectorProps['t']
  exemptions: readonly ExemptionWire[] | null
  fingerprints: ReadonlyMap<string, string | null>
  onRevoke: (entry: ExemptionWire) => void
}): ReactNode {
  return (
    <div id="mcp-panel-exemptions" role="tabpanel" aria-labelledby="mcp-tab-exemptions">
      {exemptions === null && (
        <p className="mcp-connector-note" aria-busy="true">
          <TextShimmer active>{t('loading')}</TextShimmer>
        </p>
      )}
      {exemptions !== null && exemptions.length === 0 && (
        <p className="mcp-connector-note">{t('noExemptions')}</p>
      )}
      {exemptions !== null && exemptions.length > 0 && (
        <table className="mcp-connector-table">
          <thead>
            <tr>
              <th scope="col">{t('exemption.server')}</th>
              <th scope="col">{t('exemption.tool')}</th>
              <th scope="col">{t('exemption.fingerprint')}</th>
              <th scope="col">{t('exemption.grantedAt')}</th>
              <th scope="col">{t('exemption.rule')}</th>
              <th scope="col">{t('exemption.fragment')}</th>
              <th scope="col" aria-label={t('revokeExemption')} />
            </tr>
          </thead>
          <tbody>
            {exemptions.map((entry) => {
              const current = fingerprints.get(entry.server_id)
              const stale = current !== undefined && current !== entry.fingerprint
              return (
                <tr key={`${entry.server_id}:${entry.fingerprint}:${entry.dispatch_name}`}>
                  <td>{entry.server_id}</td>
                  <td><code className="mcp-connector-mono">{entry.dispatch_name}</code></td>
                  <td>
                    <code className="mcp-connector-mono">{shortFingerprint(entry.fingerprint)}</code>
                    {stale && <Tag tone="quiet">{t('exemption.stale')}</Tag>}
                  </td>
                  <td>{formatTime(entry.granted_at_ms)}</td>
                  <td><code className="mcp-connector-mono">{entry.rule}</code></td>
                  <td className="mcp-connector-fragment">{entry.hit_fragment}</td>
                  <td>
                    <Button size="sm" variant="outline" onClick={() => { onRevoke(entry) }}>
                      {t('revokeExemption')}
                    </Button>
                  </td>
                </tr>
              )
            })}
          </tbody>
        </table>
      )}
    </div>
  )
}
