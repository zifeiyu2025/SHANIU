/**
 * 设置页服务商卡片里的能力区块（宿主插件，非 vendored）。
 *
 * 为什么占 `settings.models.provider-card`：上游 slot-contract 明确把这个槽位定义为
 * "外部插件往模型设置页加 UI 的座位"（按 `settingsNs` 键控）。上游模型行本身只编辑
 * 值（contextWindow / maxTokens / 输入模态 / 推理档），**不显示"谁说的、测过没有"**；
 * 这两样正是 SH-132 要补的，所以走扩展点，不改 vendored 组件——否则每次跟上游同步都会丢。
 *
 * 展示口径：
 *  - 值来自视图（`capability::model_view`），不在这里重新推导；
 *  - 来源徽标（内置表/端点/手动/推测/实测）与实测三态并列，`mismatch` 必须显眼；
 *  - 内置表来源旁显示数据日期，表过时要看得出来；
 *  - 实测证据放 `title`，不折叠成"通过"两个字。
 */
import { useEffect, useState, type ReactNode } from 'react'
import type { PropsLocale, PropsRuntime } from '@deepseek-ai/dsh-client-ui-slots'
import type {} from '@deepseek-ai/dsh-client-ui-settings-models/client'
import { NS, type CapabilitiesKey } from './locales'
import type { CapabilitiesWire, CapabilityModelWire, CapabilityViewWire } from './wire'
import './capabilities.css'

/** 子集：与宿主 `capability::ITEMS` 的 id 一一对应（新增能力项时两处一起加）。 */
const ITEMS = ['vision', 'tools', 'reasoning', 'context_window', 'max_output', 'builtin_web_search'] as const
type ItemId = typeof ITEMS[number]

const ITEM_LABEL: Record<ItemId, CapabilitiesKey> = {
  vision: 'item.vision',
  tools: 'item.tools',
  reasoning: 'item.reasoning',
  context_window: 'item.context_window',
  max_output: 'item.max_output',
  builtin_web_search: 'item.builtin_web_search',
}

const SOURCE_LABEL: Record<string, CapabilitiesKey> = {
  unknown: 'source.unknown',
  endpoint: 'source.endpoint',
  builtin: 'source.builtin',
  inferred: 'source.inferred',
  probe: 'source.probe',
  manual: 'source.manual',
}

const STATE_LABEL: Record<string, CapabilitiesKey> = {
  verified: 'state.verified',
  mismatch: 'state.mismatch',
  probing: 'state.probing',
  unverified: 'state.unverified',
}

/** 「检测中」的轮询间隔：探针本身有 30s 总超时，2s 一取不会打爆宿主命令。 */
const PROBING_POLL_MS = 2_000

type Props = PropsRuntime<'settings.models.provider-card'> & PropsLocale<typeof NS> & {
  readonly wire: CapabilitiesWire
  readonly subscribe: (handler: () => void) => () => void
}

/** 千分位：上下文窗口这类数字要能一眼读出量级。 */
function formatCount(value: number): string {
  return String(value).replace(/\B(?=(\d{3})+(?!\d))/g, ',')
}

/** 内置表数据日期（毫秒 → YYYY-MM-DD）；缺省/0 视为没有。 */
function dateOf(ms: number | undefined): string | undefined {
  if (ms === undefined || ms <= 0) return undefined
  return new Date(ms).toISOString().slice(0, 10)
}

function valueText(
  item: ItemId,
  view: CapabilityViewWire,
  t: (key: CapabilitiesKey, params?: Record<string, unknown>) => string,
): string {
  if (item === 'reasoning') {
    const levels = view.verifiedLevels?.length ? view.verifiedLevels : view.levels ?? []
    return levels.length > 0 ? levels.join('/') : t('value.no')
  }
  if (item === 'context_window' || item === 'max_output') {
    return typeof view.value === 'number' && view.value > 0 ? formatCount(view.value) : t('value.none')
  }
  if (typeof view.value === 'boolean') return view.value ? t('value.yes') : t('value.no')
  // 没声明时的值与"声明了没有"是两件事：写"无"会被读成"明确没有"。
  return '—'
}

export function CapabilityPanel(props: Props): ReactNode {
  const { provider, t, wire, subscribe } = props
  const [models, setModels] = useState<readonly CapabilityModelWire[]>()
  const [error, setError] = useState<string>()
  const [tick, setTick] = useState(0)

  useEffect(() => {
    let alive = true
    const load = async (): Promise<void> => {
      try {
        const providers = await wire.providers()
        if (!alive) return
        const row = providers.find(item => item.id === provider.provider)
          ?? providers.find(item => item.label !== undefined && item.label === provider.displayName)
        setModels(row?.models ?? [])
        setError(undefined)
      } catch (reason) {
        if (!alive) return
        // 读不到就如实说，不显示成"没有能力"——那是把失败伪装成结论。
        setError(reason instanceof Error ? reason.message : String(reason))
      }
    }
    void load()
    const off = subscribe(() => { void load() })
    return () => {
      alive = false
      off()
    }
  }, [wire, subscribe, provider.provider, provider.displayName, tick])

  // 探针在后台跑，宿主不会为"测完了"发广播（那会把 UI 绑到后台任务上），
  // 所以只要还有项在「检测中」，就自己轮询到结论回来为止。
  const probing = models?.some(model =>
    Object.values(model.capabilities ?? {}).some(view => view.state === 'probing'),
  ) ?? false
  useEffect(() => {
    if (!probing) return
    const timer = setTimeout(() => setTick(value => value + 1), PROBING_POLL_MS)
    return () => clearTimeout(timer)
  }, [probing, models, tick])

  if (error !== undefined) {
    return <p className="shaniu-capabilities-error" role="status">{t('loadFailed', { error })}</p>
  }
  if (models === undefined || models.length === 0) return null

  return (
    <section className="shaniu-capabilities" aria-label={t('title')}>
      <div className="shaniu-capabilities-head">
        <span className="shaniu-capabilities-title">{t('title')}</span>
        <span className="shaniu-capabilities-hint">{t('hint')}</span>
      </div>
      {/* 检测会真打请求、烧 token：成本量级必须写在用户看得见的地方，不能默认"免费"。 */}
      <p className="shaniu-capabilities-cost" title={t('costDetail')}>{t('cost')}</p>
      {models.map(model => {
        const capabilities = model.capabilities ?? {}
        const date = dateOf(
          ITEMS.map(item => capabilities[item]?.catalogUpdatedAt).find(value => value !== undefined),
        )
        return (
          <div className="shaniu-capabilities-model" key={model.id}>
            <div className="shaniu-capabilities-model-head">
              <span className="shaniu-capabilities-model-name">
                {model.name !== undefined && model.name !== '' ? model.name : model.id}
              </span>
              {date !== undefined
                ? <span className="shaniu-capabilities-date">{t('dataDate', { date })}</span>
                : null}
            </div>
            <div className="shaniu-capabilities-items">
              {ITEMS.map(item => {
                const view = capabilities[item]
                if (view === undefined) return null
                const state = view.state ?? 'unverified'
                const source = view.source ?? 'unknown'
                return (
                  <span
                    className="shaniu-capabilities-chip"
                    data-state={state}
                    data-item={item}
                    key={item}
                    title={view.evidence !== undefined && view.evidence !== ''
                      ? t('evidence', { evidence: view.evidence })
                      : undefined}
                  >
                    <span className="shaniu-capabilities-label">{t(ITEM_LABEL[item])}</span>
                    <span className="shaniu-capabilities-value">{valueText(item, view, t)}</span>
                    <span className="shaniu-capabilities-source" data-source={source}>
                      {t(SOURCE_LABEL[source] ?? 'source.unknown')}
                    </span>
                    <span className="shaniu-capabilities-state">{t(STATE_LABEL[state] ?? 'state.unverified')}</span>
                  </span>
                )
              })}
            </div>
          </div>
        )
      })}
    </section>
  )
}
