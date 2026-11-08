/**
 * SH-063 设置页 · MCP 服务器信任。
 *
 * 首连审查的审批对象是**指纹**：可执行文件 + SHA-256、argv、cwd、env 摘要、
 * 协议版本与完整工具清单。逐工具风险档位只能由用户指定（默认 destructive）；
 * 服务器自报的 `readOnlyHint` 仅展示，不参与决策（ADR-107）。
 *
 * 工具清单增删会改指纹 → 回到本页重新审查；仅描述/ Schema 变化不重批。
 * 注入命中可在读完片段后显式启用，启用记录可在此撤销（A-R4 的正向出口）。
 */
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from './locales'
import { tauriBridge } from '../transport/tauri'
import {
  mcpConnectAndReview,
  mcpListServers,
  mcpResolveTrust,
  mcpRevokeExemption,
  mcpRevokeTrust,
  tauriMcpSource,
  type McpRiskLevel,
  type McpServerSummaryWire,
  type ReviewPayloadWire,
  type ReviewToolWire,
} from '../transport/mcp-wire'
import {
  approvedCount,
  buildDecisions,
  defaultSelections,
  needsInjectionConsent,
  SELECTABLE_RISKS,
  shortFingerprint,
  type ToolSelection,
} from './model'
import './mcp-trust.css'

type McpTrustSectionProps = PropsRuntime<'settings.section'> & PropsLocale<'shaniuMcp'>

/** 把未知异常收敛成可展示文本（IPC 拒绝值可能是字符串）。 */
function errorMessage(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause)
}

/** 状态徽标文案（`disabled` 来自未启用的配置项）。 */
function statusKey(status: string): Parameters<PropsLocale<'shaniuMcp'>['t']>[0] {
  switch (status) {
    case 'disabled':
      return 'status.disabled'
    case 'review':
      return 'status.review'
    case 'trusted':
      return 'status.trusted'
    case 'blocked':
      return 'status.blocked'
    case 'degraded':
      return 'status.degraded'
    case 'failed':
      return 'status.failed'
    default:
      return 'status.idle'
  }
}

/**
 * MCP 信任设置页。
 * @param props - 槽位运行时（含 `close`）与词典。
 * @returns 设置页内容。
 */
export function McpTrustSection({ t, close }: McpTrustSectionProps): ReactNode {
  const [servers, setServers] = useState<readonly McpServerSummaryWire[]>([])
  const [review, setReview] = useState<ReviewPayloadWire | null>(null)
  const [selections, setSelections] = useState<Record<string, ToolSelection>>({})
  const [busy, setBusy] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const present = useCallback((payload: ReviewPayloadWire): void => {
    setReview(payload)
    setSelections(defaultSelections(payload))
  }, [])

  const refresh = useCallback(async (): Promise<void> => {
    try {
      setServers(await mcpListServers(tauriBridge()))
    } catch (cause) {
      setError(errorMessage(cause))
    }
  }, [])

  useEffect(() => {
    void refresh()
    return tauriMcpSource().listen((event) => {
      if (event.type === 'manifest_review') present(event.review)
      void refresh()
    })
  }, [present, refresh])

  const connect = async (serverId: string): Promise<void> => {
    setBusy(serverId)
    setError(null)
    try {
      present(await mcpConnectAndReview(tauriBridge(), serverId))
    } catch (cause) {
      setError(errorMessage(cause))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const resolve = async (approveAll: boolean): Promise<void> => {
    if (review === null) return
    const source = approveAll
      ? selections
      : Object.fromEntries(
          Object.entries(selections).map(([name, selection]) => [name, { ...selection, approved: false }]),
        )
    setBusy(review.server_id)
    setError(null)
    try {
      present(
        await mcpResolveTrust(
          tauriBridge(),
          review.server_id,
          review.fingerprint,
          buildDecisions(review, source),
        ),
      )
    } catch (cause) {
      setError(errorMessage(cause))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const revokeTrust = async (serverId: string): Promise<void> => {
    setBusy(serverId)
    setError(null)
    try {
      await mcpRevokeTrust(tauriBridge(), serverId)
      setReview(null)
    } catch (cause) {
      setError(errorMessage(cause))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const revokeExemption = async (serverId: string, dispatchName: string): Promise<void> => {
    setBusy(serverId)
    setError(null)
    try {
      await mcpRevokeExemption(tauriBridge(), serverId, dispatchName)
      // 撤销立即生效；重新拉一次审查载荷以拿到最新豁免表。
      const next = await mcpConnectAndReview(tauriBridge(), serverId)
      present(next)
    } catch (cause) {
      setError(errorMessage(cause))
    } finally {
      setBusy(null)
      void refresh()
    }
  }

  const setSelection = (name: string, patch: Partial<ToolSelection>): void => {
    setSelections((current) => {
      const previous = current[name] ?? { approved: false, risk: 'destructive', enableInjection: false }
      return { ...current, [name]: { ...previous, ...patch } }
    })
  }

  return (
    <div className="mcp-trust">
      <header className="mcp-trust-head">
        <div>
          <h2>{t('title')}</h2>
          <p className="mcp-trust-desc">{t('description')}</p>
        </div>
        <button type="button" className="mcp-btn" onClick={() => { void refresh() }}>
          {t('refresh')}
        </button>
      </header>

      {error !== null && (
        <div role="alert" className="mcp-alert">
          {t('actionFailed', { message: error })}
        </div>
      )}

      {review !== null ? (
        <ReviewPanel
          t={t}
          review={review}
          selections={selections}
          busy={busy === review.server_id}
          onSelect={setSelection}
          onApprove={() => { void resolve(true) }}
          onBlock={() => { void resolve(false) }}
          onRevokeExemption={(dispatchName) => { void revokeExemption(review.server_id, dispatchName) }}
          onClose={() => { setReview(null); close() }}
        />
      ) : (
        <ServerList
          t={t}
          servers={servers}
          busy={busy}
          onConnect={(serverId) => { void connect(serverId) }}
          onRevoke={(serverId) => { void revokeTrust(serverId) }}
        />
      )}
    </div>
  )
}

function ServerList({ t, servers, busy, onConnect, onRevoke }: {
  t: McpTrustSectionProps['t']
  servers: readonly McpServerSummaryWire[]
  busy: string | null
  onConnect(serverId: string): void
  onRevoke(serverId: string): void
}): ReactNode {
  if (servers.length === 0) {
    return <p className="mcp-empty">{t('empty')}</p>
  }
  return (
    <ul className="mcp-servers">
      {servers.map((server) => {
        const working = busy === server.id
        const canTrust = server.status === 'trusted' || server.status === 'review' || server.status === 'blocked'
        return (
          <li key={server.id} className="mcp-server">
            <div className="mcp-server-line">
              <strong>{server.id}</strong>
              <span className={`mcp-badge mcp-badge-${server.status}`}>{t(statusKey(server.status))}</span>
              <span className="mcp-server-cmd">
                {server.command} {server.args.join(' ')}
              </span>
              {server.fingerprint !== null && (
                <span className="mcp-mono" title={server.fingerprint}>
                  {t('fingerprint')}: {shortFingerprint(server.fingerprint)}
                </span>
              )}
            </div>
            {server.approved_tools.length > 0 && (
              <p className="mcp-approved">
                {t('approvedTools')}: {server.approved_tools.map((tool) => `${tool.dispatch_name}(${tool.risk})`).join(', ')}
              </p>
            )}
            {server.reason !== null && <p className="mcp-reason">{server.reason}</p>}
            <div className="mcp-actions">
              <button
                type="button"
                className="mcp-btn mcp-btn-primary"
                disabled={!server.enabled || working}
                onClick={() => { onConnect(server.id) }}
              >
                {working ? t('busy') : server.status === 'review' ? t('review') : t('connect')}
              </button>
              {canTrust && (
                <button
                  type="button"
                  className="mcp-btn mcp-btn-danger"
                  disabled={working}
                  onClick={() => { onRevoke(server.id) }}
                >
                  {t('revoke')}
                </button>
              )}
            </div>
          </li>
        )
      })}
    </ul>
  )
}

function ReviewPanel({ t, review, selections, busy, onSelect, onApprove, onBlock, onRevokeExemption, onClose }: {
  t: McpTrustSectionProps['t']
  review: ReviewPayloadWire
  selections: Record<string, ToolSelection>
  busy: boolean
  onSelect(name: string, patch: Partial<ToolSelection>): void
  onApprove(): void
  onBlock(): void
  onRevokeExemption(dispatchName: string): void
  onClose(): void
}): ReactNode {
  const checked = approvedCount(selections)
  return (
    <section className="mcp-review" aria-label={t('title')}>
      <div className="mcp-server-line">
        <strong>{review.server_id}</strong>
        <span className={`mcp-badge mcp-badge-${review.status}`}>{t(statusKey(review.status))}</span>
        <span className="mcp-mono">{t('fingerprint')}: {review.fingerprint}</span>
      </div>

      <dl className="mcp-identity">
        <dt>{t('executable')}</dt>
        <dd className="mcp-mono">{review.executable}</dd>
        <dt>{t('executableSha')}</dt>
        <dd className="mcp-mono">{review.executable_sha256}</dd>
        <dt>{t('argv')}</dt>
        <dd className="mcp-mono">{review.argv.join(' ')}</dd>
        <dt>{t('cwd')}</dt>
        <dd className="mcp-mono">{review.cwd}</dd>
        <dt>{t('protocol')}</dt>
        <dd>{review.protocol_version}</dd>
        <dt>{t('serverName')}</dt>
        <dd>{review.server_name} {review.server_version}</dd>
        <dt>{t('env')}</dt>
        <dd className="mcp-mono">{review.env.length === 0 ? '—' : review.env.join('\n')}</dd>
        <dt>{t('network')}</dt>
        <dd>{review.allow_network ? '✓' : '✗'} · {t('unsandboxed')} {review.unsandboxed ? '✓' : '✗'}</dd>
      </dl>

      {review.warnings.length > 0 && (
        <div className="mcp-warn">
          <strong>{t('warnings')}</strong>
          <ul>{review.warnings.map((warning) => <li key={warning}>{warning}</li>)}</ul>
        </div>
      )}

      <h3>{t('tools')}</h3>
      <p className="mcp-hint">{t('approveHint')}</p>
      <ul className="mcp-tools">
        {review.tools.map((tool) => (
          <ToolRow
            key={tool.dispatch_name}
            t={t}
            tool={tool}
            selection={selections[tool.dispatch_name] ?? { approved: false, risk: 'destructive', enableInjection: false }}
            onSelect={(patch) => { onSelect(tool.dispatch_name, patch) }}
          />
        ))}
      </ul>

      {review.rejected_tools.length > 0 && (
        <>
          <h3>{t('rejected')}</h3>
          <ul className="mcp-tools">
            {review.rejected_tools.map((tool) => (
              <li key={tool.raw_name} className="mcp-tool">
                <code>{tool.raw_name}</code> <span className="mcp-reason">{tool.reason}</span>
              </li>
            ))}
          </ul>
        </>
      )}

      {review.exemptions.length > 0 && (
        <>
          <h3>{t('exemptions')}</h3>
          <ul className="mcp-exemptions">
            {review.exemptions.map((entry) => (
              <li key={`${entry.fingerprint}:${entry.dispatch_name}`} className="mcp-exemption">
                <div>
                  <code>{entry.dispatch_name}</code>
                  <span className="mcp-reason">{entry.rule}: {entry.hit_fragment}</span>
                  <span className="mcp-mono">{t('fingerprint')}: {shortFingerprint(entry.fingerprint)}</span>
                </div>
                <button
                  type="button"
                  className="mcp-btn mcp-btn-danger"
                  disabled={busy}
                  onClick={() => { onRevokeExemption(entry.dispatch_name) }}
                >
                  {t('revokeExemption')}
                </button>
              </li>
            ))}
          </ul>
        </>
      )}

      <footer className="mcp-actions">
        <button
          type="button"
          className="mcp-btn mcp-btn-primary"
          disabled={busy || checked === 0}
          onClick={onApprove}
        >
          {busy ? t('busy') : `${t('approve')} (${checked})`}
        </button>
        <button type="button" className="mcp-btn mcp-btn-danger" disabled={busy} onClick={onBlock}>
          {t('block')}
        </button>
        <button type="button" className="mcp-btn" disabled={busy} onClick={onClose}>
          {t('close')}
        </button>
      </footer>
    </section>
  )
}

function ToolRow({ t, tool, selection, onSelect }: {
  t: McpTrustSectionProps['t']
  tool: ReviewToolWire
  selection: ToolSelection
  onSelect(patch: Partial<ToolSelection>): void
}): ReactNode {
  const consent = needsInjectionConsent(tool)
  return (
    <li className="mcp-tool">
      <label className="mcp-tool-head">
        <input
          type="checkbox"
          checked={selection.approved}
          onChange={(event) => { onSelect({ approved: event.target.checked }) }}
        />
        <code>{tool.dispatch_name}</code>
        <select
          value={selection.risk}
          disabled={!selection.approved}
          aria-label={`${tool.dispatch_name} risk`}
          onChange={(event) => { onSelect({ risk: event.target.value as McpRiskLevel }) }}
        >
          {SELECTABLE_RISKS.map((risk) => <option key={risk} value={risk}>{t(`risk.${risk}`)}</option>)}
        </select>
      </label>
      {tool.description !== '' && <p className="mcp-desc">{tool.description}</p>}
      {tool.server_read_only_hint && <p className="mcp-hint">{t('serverHint')}</p>}

      {tool.guard_hits.length > 0 && (
        <div className="mcp-warn">
          <strong>{t('injection')}</strong>
          <ul>
            {tool.guard_hits.map((hit) => (
              <li key={`${hit.rule}:${hit.fragment}`}><code>{hit.rule}</code>: {hit.fragment}</li>
            ))}
          </ul>
          {tool.guard_explanation !== undefined && <p className="mcp-desc">{tool.guard_explanation}</p>}
          {consent && (
            <label className="mcp-consent">
              <input
                type="checkbox"
                checked={selection.enableInjection}
                disabled={!selection.approved}
                onChange={(event) => { onSelect({ enableInjection: event.target.checked }) }}
              />
              {t('enableInjection')}
            </label>
          )}
          {tool.guard_exempted && <p className="mcp-hint">{t('exempted')}</p>}
        </div>
      )}
    </li>
  )
}
